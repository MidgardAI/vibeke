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
    /// A workspace rooted at its own new directory (distinct consent scope).
    fn workspace_at(&self, name: &str) -> (String, String) {
        let root = self.dir.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        let v = self.json(&["workspace", "create", "--cwd", &root.to_string_lossy()]);
        (
            v["workspace"]["id"]
                .as_str()
                .or(v["id"].as_str())
                .unwrap_or_else(|| v["root_pane"]["workspace"].as_str().unwrap())
                .to_string(),
            v["root_pane"]["id"].as_str().unwrap().to_string(),
        )
    }
    /// Start the hook-reporting fake agent in `pane` and send it one request; returns the run
    /// and the recorded turn number.
    fn agent(&self, pane: &str, sid: &str, request: &str) -> (String, u64) {
        let script = self.dir.path().join("fake-claude");
        if !script.exists() {
            std::fs::write(&script, FAKE_CLAUDE).unwrap();
            std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        let _ = self
            .cmd(&[
                "pane",
                "wait-idle",
                pane,
                "--quiet-ms",
                "500",
                "--timeout-ms",
                "10000",
            ])
            .output();
        self.json(&[
            "pane",
            "run",
            pane,
            &format!("FAKE_SID={sid} {}", script.to_string_lossy()),
        ]);
        let run = self.until("hook-bound run", 15, || {
            let v = self.api("task.sources", json!({"pane": pane})).ok()?;
            (v["identity_verified"] == true).then(|| v["run"].as_str().unwrap().to_string())
        });
        self.json(&["pane", "run", pane, request]);
        let n = self.until("recorded turn", 10, || {
            let v = self.api("task.sources", json!({"run": run})).ok()?;
            v["turns"].as_array()?.first()?["n"].as_u64()
        });
        (run, n)
    }
    fn consent_file(&self) -> std::path::PathBuf {
        self.dir.path().join("state").join("assistant-consent.json")
    }
    fn generate(&self, p: Value) -> Value {
        self.api("assistant.generate", p).unwrap()
    }
    fn confirm(&self, g: &Value) -> Result<Value, Value> {
        self.api(
            "assistant.confirm",
            json!({"request": g["request"]["id"], "preview_digest": g["preview"]["digest"]}),
        )
    }
    fn used(&self) -> Value {
        self.json(&["assist", "status"])["today"]["used"].clone()
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
    // Invalid output gets exactly one repair attempt (14, lane 2D); the repair cites the
    // unsent source again, so the request fails as invalid output after two calls.
    for _ in 0..2 {
        fake.push(Reply::anthropic(
            r#"{"title": "x", "criteria": [{"text": "y", "source_refs": ["s42"]}]}"#,
            1,
            1,
        ));
    }
    s.json(&[
        "assist",
        "confirm",
        &id,
        g["preview"]["digest"].as_str().unwrap(),
    ]);
    let failed = s.wait_state(&id, &["done", "failed"]);
    assert_eq!(failed["state"], "failed", "{failed}");
    assert_eq!(failed["error"]["category"], "invalid_output", "{failed}");
    assert_eq!(failed["attempts"], 2, "{failed}");
    assert_eq!(fake.count(), 3);
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

fn suggest_reply(run_turn_source: &str) -> Reply {
    Reply::anthropic(
        &json!({
            "title": "Fix login redirect",
            "objective": "Return users to the page they came from",
            "constraints": [{"text": "Draft PR only", "source_refs": [run_turn_source]}],
            "criteria": [
                {"text": "Redirect regression test passes", "evaluation": "check", "required": true, "source_refs": [run_turn_source]},
                {"text": "PR is opened as a draft", "evaluation": "external", "source_refs": []},
            ],
            "stop_at": "draft_pr",
        })
        .to_string(),
        40,
        60,
    )
}

/// Finding 11: Suggest → fill the Track form (the TUI's own code) → save → read the intent.
#[test]
fn suggested_details_keep_their_semantics_through_track() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let (ws, pane) = s.workspace_at("track");
    s.config(&fake.url(), "");
    s.json(&["assist", "consent", &ws]);
    let (run, n) = s.agent(&pane, "sess-t", "Fix the login redirect. Draft PR only.");
    let g = s.generate(json!({"operation": "suggest_task_details", "run": run, "turns": [n]}));
    fake.push(suggest_reply("s1"));
    s.confirm(&g).unwrap();
    let done = s.wait_state(g["request"]["id"].as_str().unwrap(), &["done", "failed"]);
    assert_eq!(done["state"], "done", "{done}");

    let mut form = vk_tui::tasks::TrackForm::new(1, 0, &run, &pane, "track-idem".into());
    form.load_sources(&s.api("task.sources", json!({"run": run})).unwrap());
    vk_tui::assist::apply_to_track(&mut form, &done["output"], &done["sources"]);
    let tracked = s.api("task.track", form.params()).unwrap();
    let task = tracked["task"]["id"]
        .as_str()
        .or(tracked["task"].as_str())
        .unwrap()
        .to_string();
    let intent = s.api("task.intent.get", json!({"task": task})).unwrap()["intent"].clone();
    let crit = intent["criteria"].as_array().unwrap();
    assert_eq!(crit.len(), 2, "{intent}");
    // Generated criteria stay optional (the model's `required: true` never counts) and keep
    // their evaluation kind and the turn they cite.
    assert_eq!(crit[0]["text"], "Redirect regression test passes");
    assert_eq!(crit[0]["required"], false, "{intent}");
    assert_eq!(crit[0]["evaluation"], "check");
    assert_eq!(crit[0]["source_refs"][0]["turn"], n, "{intent}");
    assert_eq!(crit[1]["required"], false);
    assert_eq!(crit[1]["evaluation"], "external");
    // Constraints stay constraints (never criteria) with their source.
    let cons = intent["constraints"].as_array().unwrap();
    assert_eq!(cons.len(), 1, "{intent}");
    assert_eq!(cons[0]["text"], "Draft PR only");
    assert_eq!(cons[0]["source_refs"][0]["turn"], n);
    assert_eq!(intent["stop_at"], "draft_pr");
}

/// Finding 1: each selected run/pane/task (and a handoff's bound runs) needs the consent of
/// the workspace it belongs to; a cross-workspace selection never auto-sends.
#[test]
fn selections_need_the_consent_of_their_own_workspace() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let (wa, pa) = s.workspace_at("ws-a");
    let (wb, pb) = s.workspace_at("ws-b");
    s.config(
        &fake.url(),
        r#"auto_send = ["pane_title", "handoff", "review_summary"]"#,
    );
    s.json(&[
        "assist",
        "consent",
        &wa,
        "--auto-send",
        "pane_title,handoff,review_summary",
    ]);
    let (run_a, _) = s.agent(&pa, "sess-a", "Work in A");
    let (run_b, _) = s.agent(&pb, "sess-b", "Secret work in B");

    let refused = |p: Value| {
        let e = s.api("assistant.generate", p.clone()).unwrap_err();
        assert_eq!(reason(&e), "consent_required", "{p}: {e}");
        assert!(e.to_string().contains("belongs to workspace"), "{e}");
    };
    // Explicit workspace A with B's pane, B's run.
    refused(json!({"operation": "pane_title", "workspace": wa, "pane": pb}));
    refused(json!({"operation": "suggest_task_details", "workspace": wa, "run": run_b}));
    refused(json!({"operation": "handoff", "workspace": wa, "run": run_b}));
    // A task tracked in B, summarized "for" A.
    let tb = s
        .api(
            "task.track",
            json!({"run": run_b, "title": "B task", "idempotency_key": "t-b"}),
        )
        .unwrap();
    let task_b = tb["task"]["id"]
        .as_str()
        .or(tb["task"].as_str())
        .unwrap()
        .to_string();
    refused(json!({"operation": "review_summary", "workspace": wa, "task": task_b}));
    // A task in A with a run from B bound to it: the handoff reads that run too.
    let t = s
        .api(
            "task.track",
            json!({"run": run_a, "title": "A task", "idempotency_key": "t-a"}),
        )
        .unwrap();
    let task = t["task"]["id"]
        .as_str()
        .or(t["task"].as_str())
        .unwrap()
        .to_string();
    let pb2 = s.json(&["pane", "split", &pb])["pane"]["id"]
        .as_str()
        .map(str::to_string);
    let pb2 = pb2.unwrap_or_else(|| panic!("split pane in B"));
    let (run_b2, _) = s.agent(&pb2, "sess-b2", "More B work");
    s.api("task.bind", json!({"task": task, "run": run_b2}))
        .unwrap();
    refused(json!({"operation": "handoff", "task": task}));
    assert_eq!(fake.count(), 0);

    // With B's consent too (and auto_send everywhere) it previews, never auto-sends.
    s.json(&[
        "assist",
        "consent",
        &wb,
        "--auto-send",
        "pane_title,handoff,review_summary",
    ]);
    let g = s.generate(json!({"operation": "handoff", "task": task}));
    assert_eq!(g["requires_confirmation"], true, "{g}");
    assert!(
        g["request"]["other_workspace_paths"]
            .as_array()
            .is_some_and(|a| a.len() == 1),
        "{g}"
    );
    // Same-workspace selection with both auto_send lists does auto-send.
    fake.push(Reply::anthropic(r#"{"title": "A pane"}"#, 5, 5));
    let g = s.generate(json!({"operation": "pane_title", "pane": pa}));
    assert_eq!(g["requires_confirmation"], false, "{g}");
    // Revoking B cancels the pending cross-workspace request.
    let r = s.json(&["assist", "revoke", &wb]);
    assert_eq!(r["cancelled_requests"], 1, "{r}");
}

/// Additional coverage: the auto_send config × consent matrix.
#[test]
fn auto_send_needs_both_config_and_consent() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    fake.set_fallback(briefing_reply("s1"));
    let (ws, _) = s.workspace_at("auto");
    for (cfg_on, grant_on) in [(false, false), (true, false), (false, true), (true, true)] {
        s.config(
            &fake.url(),
            if cfg_on {
                r#"auto_send = ["briefing"]"#
            } else {
                ""
            },
        );
        let mut args = vec!["assist", "consent", ws.as_str()];
        if grant_on {
            args.extend(["--auto-send", "briefing"]);
        }
        s.json(&args);
        let before = fake.count();
        let g = s.generate(json!({"operation": "briefing", "workspace": ws}));
        let auto = cfg_on && grant_on;
        assert_eq!(
            g["requires_confirmation"], !auto,
            "config {cfg_on} grant {grant_on}: {g}"
        );
        let id = g["request"]["id"].as_str().unwrap();
        if auto {
            assert_eq!(s.wait_state(id, &["done", "failed"])["auto_sent"], true);
            assert_eq!(fake.count(), before + 1);
        } else {
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(fake.count(), before, "config {cfg_on} grant {grant_on}");
            s.json(&["assist", "cancel", id]);
        }
    }
}

/// Finding 2: a queued request re-reads enabled state and consent right before dispatch.
#[test]
fn queued_requests_never_send_after_disable_or_revocation_elsewhere() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let (ws, _) = s.workspace_at("queue");
    let base = "max_concurrent_requests = 1";
    s.config(&fake.url(), base);
    s.json(&["assist", "consent", &ws]);

    // Revocation written by another session (the consent file is shared; nothing tells this
    // session): the queued request is cancelled at dispatch and never sent.
    fake.push(briefing_reply("s1").delayed(Duration::from_secs(2)));
    let a = s.generate(json!({"operation": "briefing", "workspace": ws}));
    s.confirm(&a).unwrap();
    s.until("first request at the provider", 10, || {
        (fake.count() == 1).then_some(())
    });
    let b = s.generate(json!({"operation": "briefing", "workspace": ws}));
    s.confirm(&b).unwrap();
    let saved = std::fs::read(s.consent_file()).unwrap();
    std::fs::write(s.consent_file(), r#"{"version": 1, "grants": []}"#).unwrap();
    let qb = s.wait_state(
        b["request"]["id"].as_str().unwrap(),
        &["cancelled", "done", "failed"],
    );
    assert_eq!(qb["state"], "cancelled", "{qb}");
    assert_eq!(qb["error"]["category"], "permission_denied", "{qb}");
    // The request already at the provider is aborted too: a running request is aborted when
    // its consent is revoked from any session (14 §8 as built).
    let qa = s.wait_state(
        a["request"]["id"].as_str().unwrap(),
        &["cancelled", "done", "failed"],
    );
    assert_eq!(qa["state"], "cancelled", "{qa}");
    assert_eq!(qa["error"]["category"], "permission_denied", "{qa}");
    assert_eq!(
        fake.count(),
        1,
        "the revoked request never reached the provider"
    );

    // Disable without any assistant call: the queued request is cancelled at dispatch.
    std::fs::write(s.consent_file(), saved).unwrap();
    fake.push(briefing_reply("s1").delayed(Duration::from_secs(2)));
    let a = s.generate(json!({"operation": "briefing", "workspace": ws}));
    s.confirm(&a).unwrap();
    s.until("request at the provider", 10, || {
        (fake.count() == 2).then_some(())
    });
    let b = s.generate(json!({"operation": "briefing", "workspace": ws}));
    s.confirm(&b).unwrap();
    let bid = b["request"]["id"].as_str().unwrap().to_string();
    std::fs::write(
        s.dir.path().join("config.toml"),
        std::fs::read_to_string(s.dir.path().join("config.toml"))
            .unwrap()
            .replace("enabled = true", "enabled = false"),
    )
    .unwrap();
    // Only non-assistant calls while waiting (they don't run assistant maintenance).
    let ev = s.until("queued request cancelled at dispatch", 15, || {
        s.events().into_iter().find(|e| {
            e["type"] == "assistant.request_finished"
                && e["subject"]["assistant_request"] == bid.as_str()
        })
    });
    assert_eq!(ev["data"]["state"], "cancelled", "{ev}");
    assert_eq!(ev["data"]["error_category"], "disabled", "{ev}");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(fake.count(), 2, "nothing sent after disable");
}

/// Finding 3: labels and source metadata go through the same redactor as text.
#[test]
fn run_names_with_secrets_are_redacted_everywhere() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let (ws, pane) = s.workspace_at("names");
    s.config(&fake.url(), "");
    let cfg = std::fs::read_to_string(s.dir.path().join("config.toml")).unwrap();
    std::fs::write(
        s.dir.path().join("config.toml"),
        format!("{cfg}\n[security.redact]\npatterns = [\"ACME-[0-9]{{6}}\"]\n"),
    )
    .unwrap();
    s.json(&["assist", "consent", &ws]);
    let (run, _) = s.agent(&pane, "sess-n", "hello");
    let custom = "ACME-424242";
    s.api(
        "agent.rename",
        json!({"target": run, "name": format!("bot-{SECRET}-{custom}")}),
    )
    .unwrap();
    let g = s.generate(json!({"operation": "briefing", "workspace": ws}));
    let shown = g.to_string();
    for secret in [SECRET, custom] {
        assert!(!shown.contains(secret), "preview leaks {secret}: {shown}");
    }
    assert!(
        g["preview"]["sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["label"].as_str().unwrap().contains("[REDACTED]")),
        "{g}"
    );
    fake.push(briefing_reply("s1"));
    s.confirm(&g).unwrap();
    let done = s.wait_state(g["request"]["id"].as_str().unwrap(), &["done", "failed"]);
    let stored = done.to_string();
    let wire = &fake.requests()[0].body;
    for secret in [SECRET, custom] {
        assert!(!wire.contains(secret), "payload leaks {secret}");
        assert!(!stored.contains(secret), "stored metadata leaks {secret}");
    }
}

fn anthropic_usage(text: &str, usage: Value) -> Reply {
    Reply::json(json!({
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": usage,
    }))
}

/// Findings 5 and 7: a retry is admitted like any attempt; partial usage stays conservative.
#[test]
fn retries_go_through_admission_and_partial_usage_is_conservative() {
    let brief = json!({"items": [], "coverage": "x"}).to_string();
    // 429 → success with one request per day: the retry is refused, never sent.
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![
        Reply::status(429, "").with_header("retry-after", "0"),
        Reply::anthropic(&brief, 1, 1),
    ]);
    let (ws, _) = s.workspace_at("retry");
    s.config(&fake.url(), "daily_request_limit = 1");
    s.json(&["assist", "consent", &ws]);
    let g = s.generate(json!({"operation": "briefing", "workspace": ws}));
    s.confirm(&g).unwrap();
    let r = s.wait_state(g["request"]["id"].as_str().unwrap(), &["done", "failed"]);
    assert_eq!(r["state"], "failed", "{r}");
    assert_eq!(r["error"]["category"], "budget_exhausted", "{r}");
    assert_eq!(
        fake.count(),
        1,
        "the retry exceeded the daily request limit"
    );
    assert_eq!(s.used()["requests"], 1);
    // Same with one request per minute (429 → 429).
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![
        Reply::status(429, "").with_header("retry-after", "0"),
        Reply::status(429, "").with_header("retry-after", "0"),
    ]);
    let (ws, _) = s.workspace_at("retry2");
    s.config(&fake.url(), "requests_per_minute = 1");
    s.json(&["assist", "consent", &ws]);
    let g = s.generate(json!({"operation": "briefing", "workspace": ws}));
    s.confirm(&g).unwrap();
    let r = s.wait_state(g["request"]["id"].as_str().unwrap(), &["done", "failed"]);
    assert_eq!(r["error"]["category"], "rate_limited", "{r}");
    assert_eq!(fake.count(), 1);
    assert_eq!(s.used()["requests"], 1);

    // Room for two: 429 → success counts two attempts.
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![
        Reply::status(429, "").with_header("retry-after", "0"),
        Reply::anthropic(&brief, 10, 20),
    ]);
    let (ws, _) = s.workspace_at("retry3");
    s.config(&fake.url(), "");
    s.json(&["assist", "consent", &ws]);
    let g = s.generate(json!({"operation": "briefing", "workspace": ws}));
    s.confirm(&g).unwrap();
    let r = s.wait_state(g["request"]["id"].as_str().unwrap(), &["done", "failed"]);
    assert_eq!(r["state"], "done", "{r}");
    assert_eq!(r["attempts"], 2);
    assert_eq!(fake.count(), 2);
    assert_eq!(s.used()["requests"], 2);
    assert_eq!(s.used()["tokens"], 30);

    // Partial usage: the unknown component keeps its reservation.
    let max_out = 1024;
    fake.push(anthropic_usage(&brief, json!({"input_tokens": 7})));
    let g = s.generate(json!({"operation": "briefing", "workspace": ws}));
    s.confirm(&g).unwrap();
    s.wait_state(g["request"]["id"].as_str().unwrap(), &["done", "failed"]);
    assert_eq!(s.used()["tokens"], 30 + 7 + max_out, "{}", s.used());
    let est = g["request"]["estimated_input_tokens"].as_u64().unwrap();
    fake.push(anthropic_usage(&brief, json!({"output_tokens": 9})));
    let g = s.generate(json!({"operation": "briefing", "workspace": ws}));
    s.confirm(&g).unwrap();
    s.wait_state(g["request"]["id"].as_str().unwrap(), &["done", "failed"]);
    let est2 = g["request"]["estimated_input_tokens"].as_u64().unwrap();
    assert_eq!(
        s.used()["tokens"],
        30 + 7 + max_out + est2 + 9,
        "{} (first estimate {est})",
        s.used()
    );
    // Cost is computed from the conservative components, not zeroed.
    assert!(s.used()["cost_usd"].as_f64().unwrap() > (max_out as f64 * 5.0) / 1e6);
}

fn server_pid(s: &Session) -> String {
    s.api("server.status", json!({})).unwrap()["pid"]
        .as_u64()
        .unwrap()
        .to_string()
}

/// Finding 6: a dispatched request's reservation survives a crash and is charged.
#[test]
fn restart_charges_interrupted_dispatched_requests() {
    let s = Session::new();
    let fake =
        FakeServer::start_in_thread(vec![briefing_reply("s1").delayed(Duration::from_secs(60))]);
    let (ws, _) = s.workspace_at("crash");
    s.config(&fake.url(), "");
    s.json(&["assist", "consent", &ws]);
    let g = s.generate(json!({"operation": "briefing", "workspace": ws}));
    let id = g["request"]["id"].as_str().unwrap().to_string();
    s.confirm(&g).unwrap();
    s.until("request at the provider", 10, || {
        (fake.count() == 1).then_some(())
    });
    let reserved = s.json(&["assist", "status"])["today"]["reserved"].clone();
    assert_eq!(reserved["requests"], 1, "{reserved}");
    let pid = server_pid(&s);
    let _ = Command::new("kill").args(["-9", &pid]).status();
    s.until("server gone", 10, || {
        (!Command::new("kill")
            .args(["-0", &pid])
            .status()
            .unwrap()
            .success())
        .then_some(())
    });
    // The next call starts the same session again.
    let st = s.until("server back", 20, || {
        s.cmd(&["assist", "status"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
    });
    assert_ne!(server_pid(&s), pid);
    assert_eq!(st["today"]["used"]["requests"], 1, "{st}");
    assert_eq!(
        st["today"]["used"]["tokens"], reserved["tokens"],
        "the whole reservation is charged: {st}"
    );
    let r = s.api("assistant.get", json!({"request": id})).unwrap();
    assert_eq!(r["request"]["state"], "interrupted");
    assert_eq!(fake.count(), 1, "never replayed");
}

/// Finding 8: racing confirmations / cancels / purges: one dispatch, no panic, still usable.
#[test]
fn concurrent_confirmations_dispatch_once() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    fake.set_fallback(briefing_reply("s1").delayed(Duration::from_millis(300)));
    let (ws, _) = s.workspace_at("race");
    s.config(&fake.url(), "");
    s.json(&["assist", "consent", &ws]);
    let g = s.generate(json!({"operation": "briefing", "workspace": ws}));
    let results: Vec<Result<Value, Value>> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..8).map(|_| sc.spawn(|| s.confirm(&g))).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    for e in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(reason(e), "not_awaiting_confirmation", "{e}");
    }
    s.wait_state(g["request"]["id"].as_str().unwrap(), &["done"]);
    assert_eq!(fake.count(), 1);
    // Confirm racing cancel and purge.
    for _ in 0..3 {
        let g = s.generate(json!({"operation": "briefing", "workspace": ws}));
        let id = g["request"]["id"].as_str().unwrap().to_string();
        std::thread::scope(|sc| {
            sc.spawn(|| s.confirm(&g));
            sc.spawn(|| s.api("assistant.cancel", json!({"request": id})));
            sc.spawn(|| s.api("assistant.purge", json!({"request": id})));
        });
    }
    // The coordinator is still healthy (no poisoned lock): a full round trip works.
    let g = s.generate(json!({"operation": "briefing", "workspace": ws}));
    s.confirm(&g).unwrap();
    assert_eq!(
        s.wait_state(g["request"]["id"].as_str().unwrap(), &["done", "failed"])["state"],
        "done"
    );
    assert!(s.json(&["assist", "status"])["enabled"] == true);
}

/// Additional coverage: concurrent generation with one idempotency key yields one request.
#[test]
fn idempotent_generation_under_concurrency() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let (ws, _) = s.workspace_at("idem");
    s.config(&fake.url(), "");
    s.json(&["assist", "consent", &ws]);
    let ids: Vec<String> = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..6)
            .map(|_| {
                sc.spawn(|| {
                    s.generate(json!({"operation": "briefing", "workspace": ws, "idempotency_key": "same"}))
                        ["request"]["id"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(ids.iter().all(|i| *i == ids[0]), "{ids:?}");
    let list = s.json(&["assist", "list"])["requests"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(list.len(), 1, "{list:?}");
}

/// Finding 9: abandoned previews expire on their own and preview admission is bounded.
#[test]
fn abandoned_previews_expire_and_are_bounded() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let (ws, _) = s.workspace_at("ttl");
    s.config(
        &fake.url(),
        "preview_ttl_seconds = 1\nresult_retention_hours = 0\nmax_concurrent_requests = 1\nmax_queued_requests = 1",
    );
    s.json(&["assist", "consent", &ws]);
    let a = s.generate(json!({"operation": "briefing", "workspace": ws}));
    let _b = s.generate(json!({"operation": "briefing", "workspace": ws}));
    // Bounded: a third unconfirmed preview is refused.
    let e = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap_err();
    assert_eq!(category(&e), "queue_full", "{e}");
    // No assistant call while waiting: the TTL alone expires the preview.
    let aid = a["request"]["id"].as_str().unwrap().to_string();
    let ev = s.until("preview expired", 10, || {
        s.events().into_iter().find(|e| {
            e["type"] == "assistant.request_finished"
                && e["subject"]["assistant_request"] == aid.as_str()
        })
    });
    assert_eq!(ev["data"]["state"], "cancelled", "{ev}");
    // Past retention too: the next maintenance removes the record entirely.
    std::thread::sleep(Duration::from_millis(50));
    assert!(s.api("assistant.get", json!({"request": aid})).is_err());
    let e = s.confirm(&a).unwrap_err();
    assert!(!e.is_null());
    // Room again.
    s.generate(json!({"operation": "briefing", "workspace": ws}));
    assert_eq!(fake.count(), 0);
}

/// Finding 10: neither configured values nor provider content reach error messages.
#[test]
fn errors_never_echo_config_values_or_provider_content() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let (ws, _) = s.workspace_at("errs");
    let sentinel = "sk-ant-SENTINEL-0123456789";
    // An inline credential string (wrong type) and an unparsable line holding one.
    for bad in [
        format!(
            "[assistant]\nenabled = true\n[assistant.connections.p]\nadapter = \"anthropic\"\ncredential = \"{sentinel}\"\n"
        ),
        format!("[assistant]\nenabled = true\nkey = {sentinel}\n"),
    ] {
        std::fs::write(s.dir.path().join("config.toml"), bad).unwrap();
        let st = s.api("assistant.status", json!({}));
        let shown = format!("{st:?}");
        assert!(!shown.contains("SENTINEL"), "{shown}");
        let e = s
            .api(
                "assistant.generate",
                json!({"operation": "briefing", "workspace": ws}),
            )
            .unwrap_err();
        assert!(!e.to_string().contains("SENTINEL"), "{e}");
    }
    // Provider replies with secrets and control characters in invalid enum / reference fields.
    s.config(&fake.url(), "");
    s.json(&["assist", "consent", &ws]);
    for body in [
        json!({"items": [{"text": "x", "kind": format!("{sentinel}\u{1b}[2J"), "source_refs": ["s1"]}]}),
        json!({"items": [{"text": "x", "targets": [format!("{sentinel}\u{7}")], "source_refs": ["s1"]}]}),
        json!({"items": [{"text": "x", "source_refs": [format!("{sentinel}\u{1b}]0;t")]}]}),
    ] {
        fake.push(Reply::anthropic(&body.to_string(), 1, 1));
        let g = s.generate(json!({"operation": "briefing", "workspace": ws}));
        s.confirm(&g).unwrap();
        let r = s.wait_state(g["request"]["id"].as_str().unwrap(), &["done", "failed"]);
        assert_eq!(r["state"], "failed", "{r}");
        let shown = r["error"].to_string();
        assert!(!shown.contains("SENTINEL"), "{shown}");
        assert!(
            !shown.contains("\\u001b") && !shown.contains("\\u0007"),
            "{shown}"
        );
    }
}

/// Additional coverage: every `assistant.*` method is denied to a pane-scoped caller.
#[test]
fn pane_scoped_callers_are_denied_every_assistant_method() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let (ws, pane) = s.workspace_at("scope");
    s.config(&fake.url(), "");
    s.json(&["assist", "consent", &ws]);
    let methods = [
        "assistant.status",
        "assistant.providers",
        "assistant.consent",
        "assistant.revoke",
        "assistant.generate",
        "assistant.confirm",
        "assistant.get",
        "assistant.list",
        "assistant.cancel",
        "assistant.purge",
        "assistant.models",
        "assistant.test",
        "assistant.background",
    ];
    let out = s.dir.path().join("scope-out");
    let script = s.dir.path().join("scope.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor m in {}; do\n  printf '%s ' \"$m\" >> {out}\n  \"$VIBEKE_BIN\" --json api call \"$m\" '{{\"operation\":\"briefing\",\"request\":\"x\",\"preview_digest\":\"x\",\"all\":true}}' >/dev/null 2>> {out}.err\n  tail -n 1 {out}.err | tr -d '\\n' >> {out}\n  echo >> {out}\ndone\necho DONE >> {out}\n",
            methods.join(" "),
            out = out.to_string_lossy()
        ),
    )
    .unwrap();
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
    let text = s.until("pane script finished", 30, || {
        let t = std::fs::read_to_string(&out).ok()?;
        t.contains("DONE").then_some(t)
    });
    for m in methods {
        let line = text
            .lines()
            .find(|l| l.starts_with(&format!("{m} ")))
            .unwrap_or_else(|| panic!("{m} missing in {text}"));
        assert!(
            // Refused either by the central scope check or by the assistant handler itself.
            line.contains("permission_denied")
                && (line.contains("pane-scoped") || line.contains("pane scope")),
            "{m} not denied: {line}"
        );
    }
    assert_eq!(fake.count(), 0);
    // Nothing was granted, revoked or created by the pane.
    assert_eq!(
        s.json(&["assist", "status"])["consents"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        s.json(&["assist", "list"])["requests"]
            .as_array()
            .unwrap()
            .is_empty()
    );
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
