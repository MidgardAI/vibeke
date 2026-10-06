//! Spec 14 assistance end to end against a local fake provider (no real model API is ever
//! contacted): off by default, consent per workspace, preview before send (and the preview is
//! byte-for-byte what is sent), redaction, budgets, cancellation, no background runs, drafts
//! that cannot mutate, and metadata-only audit events.

use serde_json::{Value, json};
use std::process::Command;
use std::time::{Duration, Instant};
use vk_assist::fake::{FakeServer, Reply};

const SECRET: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
const KEY: &str = "sk-ant-test-not-a-real-key";

const FAKE_CLAUDE: &str = r#"#!/bin/sh
SID=${FAKE_SID:-sess-1}
h() { printf '%s' "$2" | "$VIBEKE_BIN" hook claude "$1" >/dev/null 2>&1; }
q() { python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'; }
h SessionStart "{\"session_id\":\"$SID\",\"source\":\"startup\"}"
while true; do
  printf '\n╭──────────────────╮\n│ > '
  IFS= read -r line || exit 0
  P=$(printf '%s' "$line" | q)
  h UserPromptSubmit "{\"session_id\":\"$SID\",\"prompt\":$P}"
  echo "working on: $line"
  h Stop "{\"session_id\":\"$SID\",\"last_assistant_message\":\"done, tests passed\"}"
done
"#;

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new() -> Self {
        Session {
            dir: tempfile::Builder::new()
                .prefix("vkassist")
                .tempdir_in("/tmp")
                .unwrap(),
        }
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env("VK_TEST_ASSIST_KEY", KEY);
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
        ] {
            c.env_remove(k);
        }
        c.arg("--json").args(args);
        c
    }
    fn json(&self, args: &[&str]) -> Value {
        let out = self.cmd(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn api(&self, method: &str, p: Value) -> Result<Value, Value> {
        let out = self
            .cmd(&["api", "call", method, &p.to_string()])
            .output()
            .unwrap();
        if out.status.success() {
            Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
        } else {
            Err(serde_json::from_slice(&out.stderr)
                .unwrap_or_else(|_| json!(String::from_utf8_lossy(&out.stderr))))
        }
    }
    fn until<T>(&self, what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(150));
        }
    }
    fn config(&self, endpoint: &str, extra: &str) {
        std::fs::write(
            self.dir.path().join("config.toml"),
            format!(
                r#"
[assistant]
enabled = true
{extra}

[assistant.connections.primary]
adapter = "anthropic"
endpoint = "{endpoint}"
credential = {{ env = "VK_TEST_ASSIST_KEY" }}

[assistant.profiles.interactive]
connection = "primary"
model = "claude-haiku-4-5-20251001"
"#
            ),
        )
        .unwrap();
    }
    fn workspace(&self) -> (String, String) {
        let v = self.json(&["workspace", "create", "--cwd", "/tmp"]);
        (
            v["workspace"]["id"]
                .as_str()
                .or(v["id"].as_str())
                .unwrap_or_else(|| v["root_pane"]["workspace"].as_str().unwrap())
                .to_string(),
            v["root_pane"]["id"].as_str().unwrap().to_string(),
        )
    }
    fn wait_state(&self, id: &str, want: &[&str]) -> Value {
        self.until(&format!("request {id} in {want:?}"), 20, || {
            let r = self.api("assistant.get", json!({"request": id})).ok()?;
            want.contains(&r["request"]["state"].as_str()?)
                .then(|| r["request"].clone())
        })
    }
    fn events(&self) -> Vec<Value> {
        self.api("events.read", json!({"after": 0, "limit": 10000}))
            .unwrap()["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn category(e: &Value) -> String {
    e["error"]["details"]["category"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}
fn reason(e: &Value) -> String {
    e["error"]["details"]["reason"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn briefing_reply(source: &str) -> Reply {
    Reply::anthropic(
        &json!({"items": [{"text": "Nothing needs you right now.", "kind": "observed", "urgency": "fyi", "targets": [], "source_refs": [source]}], "coverage": "one workspace"}).to_string(),
        120,
        30,
    )
}

#[test]
fn off_by_default_consent_and_preview_before_send() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let (ws, _pane) = s.workspace();

    // Disabled by default: nothing is contacted.
    let st = s.json(&["assist", "status"]);
    assert_eq!(st["enabled"], false);
    assert_eq!(st["background"], false);
    let e = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap_err();
    assert_eq!(category(&e), "disabled", "{e}");

    // Enabled but no consent for this workspace.
    s.config(&fake.url(), "");
    let e = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap_err();
    assert_eq!(reason(&e), "consent_required", "{e}");

    // Consent without the needed context class.
    s.json(&["assist", "consent", &ws, "--classes", "selected_text"]);
    let e = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap_err();
    assert_eq!(reason(&e), "context_class_not_granted", "{e}");

    // Default classes: a preview, and still nothing sent.
    let c = s.json(&["assist", "consent", &ws]);
    assert!(c["notice"].as_str().unwrap().contains("cannot retract"));
    let g = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap();
    assert_eq!(g["requires_confirmation"], true);
    let req = g["request"].clone();
    let id = req["id"].as_str().unwrap().to_string();
    assert_eq!(req["state"], "awaiting_confirmation");
    let pv = &g["preview"];
    assert!(pv["user"].as_str().unwrap().contains("<sources>"));
    assert_eq!(pv["model"], "claude-haiku-4-5-20251001");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(fake.count(), 0, "a preview must not contact the provider");

    // Wrong digest: refused, still nothing sent.
    let e = s
        .api(
            "assistant.confirm",
            json!({"request": id, "preview_digest": "0000"}),
        )
        .unwrap_err();
    assert_eq!(reason(&e), "preview_mismatch");
    assert_eq!(fake.count(), 0);

    // Confirm: exactly the previewed payload is sent.
    fake.push(briefing_reply("s1"));
    s.json(&["assist", "confirm", &id, pv["digest"].as_str().unwrap()]);
    let done = s.wait_state(&id, &["done", "failed"]);
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["output"]["generated"], true);
    assert_eq!(
        done["output"]["items"][0]["text"],
        "Nothing needs you right now."
    );
    assert_eq!(done["usage"]["input_tokens"], 120);
    assert_eq!(done["usage"]["output_tokens"], 30);
    // Haiku 4.5: $1 / $5 per MTok.
    let cost = done["estimated_cost_usd"].as_f64().unwrap();
    assert!((cost - (120.0 + 150.0) / 1e6).abs() < 1e-12, "{cost}");
    let sent = &fake.requests()[0];
    assert_eq!(sent.path, "/v1/messages");
    assert_eq!(sent.header("x-api-key").as_deref(), Some(KEY));
    assert_eq!(sent.json()["system"], pv["system"]);
    assert_eq!(sent.json()["messages"][0]["content"], pv["user"]);
    assert_eq!(fake.count(), 1);

    // A finished request cannot be confirmed again; `show` returns the draft.
    assert!(
        s.api(
            "assistant.confirm",
            json!({"request": id, "preview_digest": pv["digest"]})
        )
        .is_err()
    );
    let shown = s.json(&["assist", "show", &id]);
    assert_eq!(shown["request"]["output"]["coverage"], "one workspace");

    // Changing the endpoint invalidates consent.
    let other = FakeServer::start_in_thread(vec![]);
    s.config(&other.url(), "");
    let e = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap_err();
    assert_eq!(reason(&e), "consent_invalidated", "{e}");

    // Revoke: back to consent_required.
    s.config(&fake.url(), "");
    let r = s.json(&["assist", "revoke", &ws]);
    assert_eq!(r["revoked"], 1);
    let e = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap_err();
    assert_eq!(reason(&e), "consent_required");

    // Purge forgets the output.
    let p = s.json(&["assist", "purge", &id]);
    assert_eq!(p["purged"], 1);
    assert!(s.api("assistant.get", json!({"request": id})).is_err());
    assert_eq!(fake.count(), 1);
    assert_eq!(other.count(), 0);
}

#[test]
fn redaction_budgets_cancellation_and_metadata_only_audit() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let (ws, pane) = s.workspace();
    s.config(&fake.url(), "");
    s.json(&[
        "assist",
        "consent",
        &ws,
        "--classes",
        "selected_text,structured_state,screen",
    ]);
    // A secret printed on screen.
    let _ = s
        .cmd(&[
            "pane",
            "wait-idle",
            &pane,
            "--quiet-ms",
            "500",
            "--timeout-ms",
            "10000",
        ])
        .output();
    s.json(&["pane", "run", &pane, &format!("echo token={SECRET}")]);
    s.until("secret on screen", 10, || {
        let t = s.api("pane.read", json!({"pane": pane})).ok()?;
        t["text"].as_str()?.contains(SECRET).then_some(())
    });

    let gen_title = || {
        s.api(
            "assistant.generate",
            json!({"operation": "pane_title", "pane": pane, "include_screen": true}),
        )
    };
    let g = gen_title().unwrap();
    let user = g["preview"]["user"].as_str().unwrap();
    assert!(!user.contains(SECRET), "preview must be redacted");
    assert!(user.contains("[REDACTED]"));
    assert!(g["preview"]["redactions"].as_u64().unwrap() >= 1);
    assert!(
        g["preview"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["kind"] == "screen")
    );
    let id = g["request"]["id"].as_str().unwrap().to_string();
    fake.push(Reply::anthropic(
        r#"{"title": "Echo a token", "method": "pane.rename", "params": {"pane": "x"}}"#,
        50,
        8,
    ));
    s.json(&[
        "assist",
        "confirm",
        &id,
        g["preview"]["digest"].as_str().unwrap(),
    ]);
    let done = s.wait_state(&id, &["done", "failed"]);
    assert_eq!(done["output"]["title"], "Echo a token");
    assert!(done["output"].get("method").is_none());
    let body = &fake.requests()[0].body;
    assert!(
        !body.contains(SECRET),
        "the provider must never see the secret"
    );
    assert!(body.contains("[REDACTED]"));
    // The suggested title is not applied.
    let p = s.json(&["pane", "get", &pane]);
    assert_ne!(p["pane"]["title"], "Echo a token");

    // Cancellation while running.
    fake.push(briefing_reply("s1").delayed(Duration::from_secs(30)));
    let g = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap();
    let slow = g["request"]["id"].as_str().unwrap().to_string();
    s.json(&[
        "assist",
        "confirm",
        &slow,
        g["preview"]["digest"].as_str().unwrap(),
    ]);
    s.wait_state(&slow, &["running"]);
    s.until("request reached the provider", 10, || {
        (fake.count() == 2).then_some(())
    });
    let t = Instant::now();
    let c = s.json(&["assist", "cancel", &slow]);
    assert_eq!(c["request"]["state"], "cancelled");
    assert!(t.elapsed() < Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        s.json(&["assist", "show", &slow])["request"]["state"],
        "cancelled"
    );
    assert!(s.api("assistant.cancel", json!({"request": slow})).is_err());

    // Token budget: the reservation (estimate + max output) does not fit.
    s.config(&fake.url(), "daily_token_limit = 100");
    let g = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap();
    let e = s
        .api(
            "assistant.confirm",
            json!({"request": g["request"]["id"], "preview_digest": g["preview"]["digest"]}),
        )
        .unwrap_err();
    assert_eq!(category(&e), "budget_exhausted", "{e}");

    // Request budget: two attempts were used today (title + cancelled briefing).
    s.config(&fake.url(), "daily_request_limit = 2");
    let e = s
        .api(
            "assistant.confirm",
            json!({"request": g["request"]["id"], "preview_digest": g["preview"]["digest"]}),
        )
        .unwrap_err();
    assert_eq!(category(&e), "budget_exhausted", "{e}");
    let st = s.json(&["assist", "status"]);
    assert_eq!(st["today"]["used"]["requests"], 2, "{st}");
    // Cost budget with known pricing.
    s.config(&fake.url(), "daily_cost_limit_usd = 0.000001");
    let e = s
        .api(
            "assistant.confirm",
            json!({"request": g["request"]["id"], "preview_digest": g["preview"]["digest"]}),
        )
        .unwrap_err();
    assert_eq!(category(&e), "budget_exhausted", "{e}");
    assert_eq!(fake.count(), 2, "refused requests never reach the provider");

    // Audit events: metadata only.
    let evs: Vec<Value> = s
        .events()
        .into_iter()
        .filter(|e| {
            e["type"]
                .as_str()
                .is_some_and(|k| k.starts_with("assistant."))
        })
        .collect();
    let kinds: Vec<&str> = evs.iter().filter_map(|e| e["type"].as_str()).collect();
    for k in [
        "assistant.consent_granted",
        "assistant.request_created",
        "assistant.request_started",
        "assistant.request_finished",
    ] {
        assert!(kinds.contains(&k), "missing {k} in {kinds:?}");
    }
    let finished = evs
        .iter()
        .find(|e| e["type"] == "assistant.request_finished" && e["data"]["state"] == "done")
        .unwrap();
    assert_eq!(finished["data"]["model"], "claude-haiku-4-5-20251001");
    assert_eq!(finished["data"]["adapter"], "anthropic");
    assert_eq!(finished["data"]["input_tokens"], 50);
    assert!(finished["data"]["estimated_cost_usd"].as_f64().is_some());
    let all = serde_json::to_string(&evs).unwrap();
    for needle in [
        SECRET,
        "[REDACTED]",
        "Echo a token",
        "<sources>",
        KEY,
        "Nothing needs you",
    ] {
        assert!(!all.contains(needle), "audit events leak `{needle}`");
    }
}

#[test]
fn suggest_task_details_without_background_runs_or_mutations() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let (ws, pane) = s.workspace();
    s.config(&fake.url(), "");
    s.json(&["assist", "consent", &ws]);
    let script = s.dir.path().join("fake-claude");
    std::fs::write(&script, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let _ = s
        .cmd(&[
            "pane",
            "wait-idle",
            &pane,
            "--quiet-ms",
            "500",
            "--timeout-ms",
            "10000",
        ])
        .output();
    s.json(&["pane", "run", &pane, &script.to_string_lossy()]);
    let run = s.until("hook-bound run", 15, || {
        let v = s.api("task.sources", json!({"pane": pane})).ok()?;
        (v["identity_verified"] == true).then(|| v["run"].as_str().unwrap().to_string())
    });
    s.json(&[
        "pane",
        "run",
        &pane,
        "Fix the login redirect. Draft PR only.",
    ]);
    let n = s.until("recorded turn", 10, || {
        let v = s.api("task.sources", json!({"run": run})).ok()?;
        v["turns"].as_array()?.first()?["n"].as_u64()
    });
    // A turn ended: no assistant request and no provider call happen on their own.
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(fake.count(), 0);
    assert!(
        s.json(&["assist", "list"])["requests"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let g = s
        .api(
            "assistant.generate",
            json!({"operation": "suggest_task_details", "run": run, "turns": [n]}),
        )
        .unwrap();
    let user = g["preview"]["user"].as_str().unwrap();
    assert!(user.contains("Fix the login redirect. Draft PR only."));
    assert!(
        !user.contains("done, tests passed"),
        "only the selected request is sent"
    );
    let id = g["request"]["id"].as_str().unwrap().to_string();
    // A model trying to drive mutations: extra fields are dropped, nothing is executed.
    fake.push(Reply::anthropic(
        &json!({
            "title": "Fix login redirect",
            "objective": "Return users to the page they came from",
            "constraints": [{"text": "Draft PR only", "source_refs": ["s1"]}],
            "criteria": [{"text": "Redirect regression test", "evaluation": "check", "required": true, "source_refs": ["s1"]}],
            "stop_at": "draft_pr",
            "method": "task.track",
            "params": {"run": run},
            "actions": [{"method": "interaction.answer", "params": {"decision": "allow"}}],
        })
        .to_string(),
        40,
        60,
    ));
    s.json(&[
        "assist",
        "confirm",
        &id,
        g["preview"]["digest"].as_str().unwrap(),
    ]);
    let done = s.wait_state(&id, &["done", "failed"]);
    let out = &done["output"];
    assert_eq!(out["title"], "Fix login redirect", "{done}");
    assert_eq!(out["stop_at"], "draft_pr");
    assert_eq!(out["criteria"][0]["required"], false);
    assert_eq!(out["constraints"][0]["source_refs"][0], "s1");
    for k in ["method", "params", "actions"] {
        assert!(out.get(k).is_none());
    }
    assert!(
        s.json(&["task", "list"])["tasks"]
            .as_array()
            .is_none_or(|a| a.is_empty()),
        "a draft never tracks a task"
    );
    assert_eq!(fake.count(), 1);

    // An output citing a source that was not sent is rejected.
    let g = s
        .api(
            "assistant.generate",
            json!({"operation": "suggest_task_details", "run": run}),
        )
        .unwrap();
    let id = g["request"]["id"].as_str().unwrap().to_string();
    fake.push(Reply::anthropic(
        r#"{"title": "x", "criteria": [{"text": "y", "source_refs": ["s42"]}]}"#,
        1,
        1,
    ));
    s.json(&[
        "assist",
        "confirm",
        &id,
        g["preview"]["digest"].as_str().unwrap(),
    ]);
    let failed = s.wait_state(&id, &["done", "failed"]);
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["error"]["category"], "invalid_output");
    assert!(failed.get("output").is_none_or(Value::is_null));

    // Idempotent submission.
    let a = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws, "idempotency_key": "k1"}),
        )
        .unwrap();
    let b = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws, "idempotency_key": "k1"}),
        )
        .unwrap();
    assert_eq!(b["deduplicated"], true);
    assert_eq!(a["request"]["id"], b["request"]["id"]);
}

fn git(repo: &std::path::Path, args: &[&str]) -> String {
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
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// 15 §8.2 (T4): the model effort estimate goes through 14's preview + confirm, comes back as
/// a labelled draft and is never applied; the user applies it with an explicit `task.set`.
#[test]
fn effort_estimate_is_previewed_confirmed_and_never_applied() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let repo = s.dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "base\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let v = s.json(&["workspace", "create", "--cwd", &repo.to_string_lossy()]);
    let ws = v["workspace"]["id"]
        .as_str()
        .or(v["id"].as_str())
        .unwrap_or_else(|| v["root_pane"]["workspace"].as_str().unwrap())
        .to_string();
    let pane = v["root_pane"]["id"].as_str().unwrap().to_string();
    s.config(&fake.url(), "");
    s.json(&["assist", "consent", &ws]);
    let script = s.dir.path().join("fake-claude");
    std::fs::write(&script, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let _ = s
        .cmd(&[
            "pane",
            "wait-idle",
            &pane,
            "--quiet-ms",
            "500",
            "--timeout-ms",
            "10000",
        ])
        .output();
    s.json(&["pane", "run", &pane, &script.to_string_lossy()]);
    let run = s.until("hook-bound run", 15, || {
        let v = s.api("task.sources", json!({"pane": pane})).ok()?;
        (v["identity_verified"] == true).then(|| v["run"].as_str().unwrap().to_string())
    });
    s.json(&["pane", "run", &pane, "Fix the redirect"]);
    s.until("recorded turn", 10, || {
        let v = s.api("task.sources", json!({"run": run})).ok()?;
        v["turns"].as_array()?.first()?["n"].as_u64()
    });
    let t = s
        .api(
            "task.track",
            json!({"run": run, "criteria": ["Looks right"], "stop_at": "implementation"}),
        )
        .unwrap();
    let task = t["task"]["id"].as_str().unwrap().to_string();
    std::fs::write(repo.join("a.txt"), "base\nfix\n").unwrap();
    git(&repo, &["commit", "-q", "-am", "fix"]);

    // The deterministic heuristic is in the package, labelled as such.
    let pkg = s.api("task.review.get", json!({"task": task})).unwrap();
    assert_eq!(
        pkg["effort"]["heuristic"]["source"], "heuristic",
        "{}",
        pkg["effort"]
    );
    assert_eq!(pkg["effort"]["heuristic"]["effort"], "quick");

    // Preview first: nothing is sent until the exact payload is confirmed.
    let g = s
        .api(
            "assistant.generate",
            json!({"operation": "effort_estimate", "task": task}),
        )
        .unwrap();
    assert_eq!(g["requires_confirmation"], true);
    let user = g["preview"]["user"].as_str().unwrap();
    assert!(
        user.contains("heuristic"),
        "the package (with the heuristic) is the input"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fake.count(), 0);
    let id = g["request"]["id"].as_str().unwrap().to_string();
    fake.push(Reply::anthropic(
        &json!({
            "effort": "deep",
            "rationale": "Touches the redirect logic; needs a careful look",
            "source_refs": ["s2"],
            "method": "task.set",
            "params": {"task": task, "effort": "deep"},
        })
        .to_string(),
        50,
        20,
    ));
    s.json(&[
        "assist",
        "confirm",
        &id,
        g["preview"]["digest"].as_str().unwrap(),
    ]);
    let done = s.wait_state(&id, &["done", "failed"]);
    let out = &done["output"];
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(out["effort"], "deep");
    assert_eq!(out["applied"], false);
    assert_eq!(out["estimate_source"], "assistant");
    assert!(out.get("method").is_none() && out.get("params").is_none());
    assert_eq!(fake.count(), 1);

    // Not applied: the task's effort is unchanged until the user sets it.
    let task_now = |s: &Session| {
        s.api("task.list", json!({})).unwrap()["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == task.as_str())
            .cloned()
            .unwrap()
    };
    assert!(task_now(&s)["effort"].is_null());
    s.api(
        "task.set",
        json!({"task": task, "effort": "deep", "effort_source": format!("assistant:{id}")}),
    )
    .unwrap();
    assert_eq!(task_now(&s)["effort"], "deep");
    let set = s
        .events()
        .into_iter()
        .rfind(|e| e["type"] == "task.updated")
        .unwrap();
    assert_eq!(set["data"]["effort_source"], format!("assistant:{id}"));
}
