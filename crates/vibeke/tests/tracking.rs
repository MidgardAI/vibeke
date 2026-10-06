//! Spec 15 T1 end to end with a fake Claude that reports through the real hook shim:
//! track after the fact, idempotent mutations, record-only intent revisions, a guarded
//! clarification send, `/clear` suspending the binding, Continue task, attached finish.

use serde_json::{Value, json};
use std::process::Command;
use std::time::{Duration, Instant};

const FAKE_CLAUDE: &str = r#"#!/bin/sh
# Minimal stand-in for an interactive harness: an input box, prompts reported via hooks.
SID=sess-1
h() { printf '%s' "$2" | "$VIBEKE_BIN" hook claude "$1" >/dev/null 2>&1; }
q() { python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'; }
h SessionStart "{\"session_id\":\"$SID\",\"source\":\"startup\"}"
while true; do
  printf '\n╭──────────────────╮\n│ > '
  IFS= read -r line || exit 0
  case "$line" in
    /clear) SID=sess-2; h SessionStart "{\"session_id\":\"$SID\",\"source\":\"clear\"}"; continue ;;
  esac
  P=$(printf '%s' "$line" | q)
  h UserPromptSubmit "{\"session_id\":\"$SID\",\"prompt\":$P}"
  h PreToolUse "{\"session_id\":\"$SID\",\"tool_name\":\"Bash\",\"tool_use_id\":\"t$$-$(date +%s%N)\",\"tool_input\":{\"command\":\"cargo test\"}}"
  echo "working on: $line"
  h Stop "{\"session_id\":\"$SID\",\"last_assistant_message\":\"done\"}"
done
"#;

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new() -> Self {
        Session {
            dir: tempfile::Builder::new()
                .prefix("vktrack")
                .tempdir_in("/tmp")
                .unwrap(),
        }
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"));
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
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
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

#[test]
fn track_after_the_fact() {
    let s = Session::new();
    let script = s.dir.path().join("fake-claude");
    std::fs::write(&script, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let pane = s.json(&["workspace", "create", "--cwd", "/tmp"])["root_pane"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let _ = s
        .cmd(&[
            "pane",
            "wait-idle",
            &pane,
            "--quiet-ms",
            "600",
            "--timeout-ms",
            "10000",
        ])
        .output();
    s.json(&["pane", "run", &pane, &script.to_string_lossy()]);
    let run = s.until("hook-bound run", 15, || {
        let v = s.api("task.sources", json!({"pane": pane})).ok()?;
        (v["identity_verified"] == true).then(|| v["run"].as_str().unwrap().to_string())
    });

    // The user talks to the harness directly. Nothing is tracked yet.
    s.json(&[
        "pane",
        "run",
        &pane,
        "Fix the login redirect. Preserve SSO behaviour.",
    ]);
    let turns = s.until("recorded turn", 10, || {
        let v = s.api("task.sources", json!({"run": run})).ok()?;
        let t = v["turns"].as_array()?.clone();
        (!t.is_empty()).then_some(t)
    });
    assert_eq!(
        turns[0]["prompt"],
        "Fix the login redirect. Preserve SSO behaviour."
    );
    assert!(
        s.json(&["task", "list"])["tasks"]
            .as_array()
            .is_none_or(|a| a.is_empty()),
        "detection must not create tasks"
    );

    // Track it, idempotently.
    let p = json!({"run": run, "criterion": ["Preserve SSO behaviour", "Add a regression test"], "idempotency_key": "track-1"});
    let tracked = s.api("task.track", p.clone()).unwrap();
    let task = tracked["task"]["id"].as_str().unwrap().to_string();
    assert_eq!(tracked["task"]["ownership"], "attached");
    assert_eq!(
        tracked["task"]["title"],
        "Fix the login redirect. Preserve SSO behaviour."
    );
    assert_eq!(tracked["intent"]["stop_at"], "unspecified");
    assert_eq!(tracked["task"]["review_label"], "needs_task_details");
    s.until("run.task projected from the binding", 5, || {
        let runs = s.json(&["agent", "list"]);
        runs["runs"]
            .as_array()?
            .iter()
            .any(|r| r["id"] == run.as_str() && r["task"] == task.as_str())
            .then_some(())
    });
    let again = s.api("task.track", p).unwrap();
    assert_eq!(again["replayed"], true);
    assert_eq!(again["task"]["id"], task.as_str());
    assert_eq!(
        s.api(
            "task.track",
            json!({"run": run, "title": "other", "idempotency_key": "track-1"})
        )
        .unwrap_err()["error"]["details"]["reason"],
        "idempotency_key_reused"
    );
    let receipt = s
        .api("task.operation.get", json!({"idempotency_key": "track-1"}))
        .unwrap();
    assert_eq!(receipt["known"], true);

    // Quoted requirement counts as communicated; the hand-added one doesn't.
    let ig = s.api("task.intent.get", json!({"task": task})).unwrap();
    let crit = ig["intent"]["criteria"].as_array().unwrap();
    let id_of = |text: &str| {
        crit.iter().find(|c| c["text"] == text).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let unc: Vec<String> = serde_json::from_value(ig["uncommunicated"].clone()).unwrap();
    assert_eq!(unc, vec![id_of("Add a regression test")]);

    // Record-only revision with an expected-revision check.
    assert!(
        s.api(
            "task.intent.update",
            json!({"task": task, "expected_revision": 7, "stop_at": "draft_pr"})
        )
        .is_err()
    );
    let up = s
        .api(
            "task.intent.update",
            json!({"task": task, "expected_revision": 1, "stop_at": "draft_pr"}),
        )
        .unwrap();
    assert_eq!(up["intent"]["revision"], 2);
    assert_eq!(up["task"]["review_label"], "in_progress");

    // Clarification: prepare (no bytes), then send through the guarded path.
    let prep = s.api("task.message.prepare", json!({"task": task, "text": "Please add the regression test first.", "communicates_intent": true})).unwrap();
    assert_eq!(prep["send_path"], "prompt_input", "{prep}");
    let mid = prep["message"]["id"].as_str().unwrap().to_string();
    s.api("task.message.send", json!({"message": mid})).unwrap();
    let st = s.until("message delivered", 15, || {
        let m = s.api("task.message.get", json!({"message": mid})).ok()?;
        let st = m["message"]["state"].as_str()?.to_string();
        (st != "sending").then_some(st)
    });
    assert_eq!(st, "delivered");
    let ig = s.api("task.intent.get", json!({"task": task})).unwrap();
    assert_eq!(
        ig["uncommunicated"].as_array().unwrap().len(),
        0,
        "delivered clarification covers the criteria"
    );

    // Unsafe send: a draft in the input box → refused with zero bytes.
    let _ = s.cmd(&["pane", "send-text", &pane, "half typed"]).output();
    std::thread::sleep(Duration::from_millis(300));
    let prep2 = s
        .api(
            "task.message.prepare",
            json!({"task": task, "text": "second"}),
        )
        .unwrap();
    let e = s
        .api(
            "task.message.send",
            json!({"message": prep2["message"]["id"]}),
        )
        .unwrap_err();
    assert_eq!(e["error"]["details"]["reason"], "send_unsafe", "{e}");
    let _ = s.cmd(&["pane", "send-keys", &pane, "ctrl+u"]).output();

    // Items observed from the hooks.
    let detail = s.api("task.detail", json!({"task": task})).unwrap();
    assert_eq!(detail["bindings"][0]["state"], "active");

    // /clear: new conversation → binding suspended; Continue task re-binds explicitly.
    s.json(&["pane", "run", &pane, "/clear"]);
    s.until("binding suspended", 10, || {
        let d = s.api("task.detail", json!({"task": task})).ok()?;
        d["bindings"]
            .as_array()?
            .iter()
            .any(|b| b["state"] == "suspended")
            .then_some(())
    });
    let cont = s
        .api("task.bind", json!({"task": task, "run": run}))
        .unwrap();
    assert_eq!(cont["binding"]["native_conversation_id"], "sess-2");

    // Finishing an attached task never touches the pane.
    let fin = s.json(&["task", "finish", &task]);
    assert_eq!(fin["task"]["review_label"], "finished_without_review");
    assert!(
        s.json(&["pane", "get", &pane])["pane"]["id"].is_string(),
        "the pane must survive"
    );
}
