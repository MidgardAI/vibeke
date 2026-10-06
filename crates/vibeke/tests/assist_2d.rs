//! Spec 14 assistance, batch 2D, end to end against local fake providers (no real model API is
//! ever contacted, no real harness is launched): model lists with provenance and capability
//! records, the explicit connection test and probes, streaming and native structured output,
//! the one bounded repair attempt, result caching under scope-bound keys, background priority
//! and coalescing, semantic navigation, decision cards with staleness, the opt-in background
//! sweeper (summaries and stall notices), remote sources, the Gemini adapter, a fake keychain,
//! disable-while-in-flight, live concurrency changes, cloud/local switching, `forget`
//! integration, doctor and latency under concurrent slow streams.

use serde_json::{Value, json};
use std::io::BufRead;
use std::process::Command;
use std::time::{Duration, Instant};
use vk_assist::fake::{FakeServer, Reply};

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

/// A fake agent whose every turn runs the same failing command three times (a repetition
/// signal), reported through the real hook shim.
const FAKE_CLAUDE_FAILING: &str = r#"#!/bin/sh
SID=${FAKE_SID:-sess-1}
h() { printf '%s' "$2" | "$VIBEKE_BIN" hook claude "$1" >/dev/null 2>&1; }
q() { python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'; }
h SessionStart "{\"session_id\":\"$SID\",\"source\":\"startup\"}"
while true; do
  printf '\n╭──────────────────╮\n│ > '
  IFS= read -r line || exit 0
  P=$(printf '%s' "$line" | q)
  h UserPromptSubmit "{\"session_id\":\"$SID\",\"prompt\":$P}"
  for i in 1 2 3; do
    h PreToolUse "{\"session_id\":\"$SID\",\"tool_name\":\"Bash\",\"tool_use_id\":\"t$i\",\"tool_input\":{\"command\":\"npm install\"}}"
    h PostToolUse "{\"session_id\":\"$SID\",\"tool_name\":\"Bash\",\"tool_use_id\":\"t$i\",\"tool_input\":{\"command\":\"npm install\"},\"tool_response\":{\"exit_code\":1}}"
  done
  h Stop "{\"session_id\":\"$SID\",\"last_assistant_message\":\"gave up\"}"
done
"#;

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new() -> Self {
        Session {
            dir: tempfile::Builder::new()
                .prefix("vkassist2d")
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
            .env("VK_TEST_ASSIST_KEY", KEY)
            .env(
                "VIBEKE_ASSISTANT_FAKE_KEYCHAIN",
                d.join("fake-keychain.json"),
            );
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
            std::thread::sleep(Duration::from_millis(120));
        }
    }
    fn write_config(&self, text: &str) {
        std::fs::write(self.dir.path().join("config.toml"), text).unwrap();
    }
    /// The standard anthropic-adapter config. `top` goes under `[assistant]`, `profile` under
    /// the interactive profile.
    fn config(&self, endpoint: &str, top: &str, profile: &str) {
        self.write_config(&cfg(true, endpoint, top, profile));
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
    fn quiet(&self, pane: &str) {
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
    }
    fn get(&self, id: &str) -> Value {
        self.api("assistant.get", json!({"request": id})).unwrap()["request"].clone()
    }
    fn wait_state(&self, id: &str, want: &[&str]) -> Value {
        self.until(&format!("request {id} in {want:?}"), 25, || {
            let r = self.api("assistant.get", json!({"request": id})).ok()?;
            want.contains(&r["request"]["state"].as_str()?)
                .then(|| r["request"].clone())
        })
    }
    /// Generate, confirm and wait for `done` (panics on any other end).
    fn run(&self, p: Value) -> Value {
        let g = self.api("assistant.generate", p).unwrap();
        let id = g["request"]["id"].as_str().unwrap().to_string();
        self.api(
            "assistant.confirm",
            json!({"request": id, "preview_digest": g["preview"]["digest"]}),
        )
        .unwrap();
        self.wait_state(&id, &["done", "failed", "cancelled"])
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
    fn script(&self, name: &str, body: &str) -> std::path::PathBuf {
        let p = self.dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        p
    }
    /// Start a hook-reporting fake agent in `pane` and send it one request; returns the run
    /// and the recorded turn number.
    fn agent(&self, pane: &str, name: &str, body: &str, sid: &str, request: &str) -> (String, u64) {
        let script = self.script(name, body);
        self.quiet(pane);
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
        // Settled: the turn has ended, so a later change of state is a real change.
        self.until("agent idle", 20, || {
            let runs = self.json(&["agent", "list"]);
            runs["runs"]
                .as_array()?
                .iter()
                .find(|r| r["id"] == run.as_str())
                .filter(|r| r["execution"]["value"] == "Idle")
                .map(|_| ())
        });
        (run, n)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn cfg(enabled: bool, endpoint: &str, top: &str, profile: &str) -> String {
    format!(
        r#"
[assistant]
enabled = {enabled}
{top}

[assistant.connections.primary]
adapter = "anthropic"
endpoint = "{endpoint}"
credential = {{ env = "VK_TEST_ASSIST_KEY" }}

[assistant.profiles.interactive]
connection = "primary"
model = "claude-haiku-4-5-20251001"
{profile}
"#
    )
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

fn briefing_ok() -> Reply {
    Reply::anthropic(
        &json!({"items": [{"text": "Nothing needs you right now.", "kind": "observed", "urgency": "fyi", "targets": [], "source_refs": ["s1"]}], "coverage": "one workspace"}).to_string(),
        120,
        30,
    )
}

fn title(t: &str) -> String {
    json!({"title": t}).to_string()
}

fn tool_stream(parts: &[&str], input: u64, output: u64) -> Reply {
    let mut p = vec![
        format!(
            "event: message_start\ndata: {}\n\n",
            json!({"type": "message_start", "message": {"usage": {"input_tokens": input, "output_tokens": 1}}})
        ),
        format!(
            "event: content_block_start\ndata: {}\n\n",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "emit_result", "input": {}}})
        ),
    ];
    for t in parts {
        p.push(format!("event: content_block_delta\ndata: {}\n\n", json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": t}})));
    }
    p.push(format!("event: message_delta\ndata: {}\n\n", json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": output}})));
    p.push(format!(
        "event: message_stop\ndata: {}\n\n",
        json!({"type": "message_stop"})
    ));
    Reply::sse(p).with_piece_delay(Duration::from_millis(60))
}

// ---- model picker and capability records (14 §5.2) ------------------------------------------------

#[test]
fn models_are_labelled_by_provenance_and_capabilities_start_unknown() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    s.config(&fake.url(), "", "");

    // Bundled: no network, no refresh time, nothing claimed beyond plain text.
    let l = s.json(&["assist", "models", "--connection", "primary"]);
    assert_eq!(l["provenance"], "bundled", "{l}");
    assert!(l["refreshed_at_ms"].is_null());
    assert!(
        l["models"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"].as_str().unwrap().contains("haiku"))
    );
    assert!(l["note"].as_str().unwrap().contains("credential"));
    let caps = &l["current_capabilities"];
    assert_eq!(l["current_model"], "claude-haiku-4-5-20251001");
    assert_eq!(caps["text"]["support"], "supported");
    for f in ["streaming", "json_schema", "tools", "images"] {
        assert_eq!(caps[f]["support"], "unknown", "{f}: {caps}");
        assert_eq!(caps[f]["source"], "bundled");
        assert!(caps[f]["verified_on"].is_null());
    }
    assert_eq!(
        fake.count(),
        0,
        "listing a bundled list never touches the network"
    );

    // Live: one GET to the provider's listing endpoint with the credential, labelled live.
    fake.push(Reply::json(json!({"data": [
        {"id": "claude-haiku-4-5-20251001", "display_name": "Haiku"},
        {"id": "claude-other", "display_name": "Other"},
    ]})));
    let live = s.json(&["assist", "models", "--connection", "primary", "--refresh"]);
    assert_eq!(live["provenance"], "live", "{live}");
    let at = live["refreshed_at_ms"].as_i64().unwrap();
    assert_eq!(live["models"].as_array().unwrap().len(), 2);
    assert_eq!(fake.count(), 1);
    let sent = &fake.requests()[0];
    assert_eq!(sent.method, "GET");
    assert_eq!(sent.path, "/v1/models?limit=100");
    assert_eq!(sent.header("x-api-key").as_deref(), Some(KEY));

    // Cached afterwards, with the original refresh time, under both spellings of the noun.
    for noun in ["assist", "assistant"] {
        let c = s.json(&[noun, "models", "--connection", "primary"]);
        assert_eq!(c["provenance"], "cached", "{c}");
        assert_eq!(c["refreshed_at_ms"].as_i64(), Some(at));
    }
    assert_eq!(fake.count(), 1);

    // Disabling stops refreshes; listing what is known still works.
    s.write_config(&cfg(false, &fake.url(), "", ""));
    let e = s
        .api(
            "assistant.models",
            json!({"connection": "primary", "refresh": true}),
        )
        .unwrap_err();
    assert_eq!(category(&e), "disabled", "{e}");
    assert_eq!(fake.count(), 1);
    assert_eq!(
        s.json(&["assist", "models", "--connection", "primary"])["provenance"],
        "cached"
    );

    // A refresh failure carries a category and never the provider's body.
    s.config(&fake.url(), "", "");
    fake.push(Reply::status(401, "{\"error\":\"secret prompt echoed\"}"));
    let e = s
        .api(
            "assistant.models",
            json!({"connection": "primary", "refresh": true}),
        )
        .unwrap_err();
    assert_eq!(category(&e), "authentication_failed", "{e}");
    assert!(!e.to_string().contains("secret prompt echoed"));
}

#[test]
fn the_connection_test_is_counted_usage_and_probes_record_what_they_observe() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![
        Reply::anthropic("{\"ok\": true}", 20, 6),
        Reply::anthropic_stream(&["{\"ok\":", " true}"], 21, 7),
        Reply::anthropic_tool(json!({"ok": true}), 22, 8),
    ]);
    s.config(&fake.url(), "", "");
    assert_eq!(s.used()["requests"], 0);
    let r = s.json(&["assist", "test", "--probe", "streaming,json_schema"]);
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["counted"], true);
    assert_eq!(r["attempts"], 3);
    assert_eq!(r["usage"]["input_tokens"], 63);
    assert_eq!(r["usage"]["output_tokens"], 21);
    assert_eq!(r["probes"][0]["feature"], "streaming");
    assert_eq!(r["probes"][0]["recorded"], "supported", "{r}");
    assert_eq!(r["probes"][1]["feature"], "json_schema");
    assert_eq!(r["probes"][1]["recorded"], "supported", "{r}");
    // Every attempt counts toward the day's allowance, like any request.
    assert_eq!(s.used()["requests"], 3);
    assert_eq!(s.used()["tokens"], 84);
    // The probes sent what they claimed: a stream flag, and a forced tool.
    let reqs = fake.requests();
    assert!(reqs[0].json().get("stream").is_none());
    assert_eq!(reqs[1].json()["stream"], true);
    assert_eq!(reqs[2].json()["tool_choice"]["type"], "tool");
    // Observed support is recorded with its source and date and survives in the picker.
    let m = s.json(&["assistant", "models", "--connection", "primary"]);
    for f in ["streaming", "json_schema"] {
        let c = &m["current_capabilities"][f];
        assert_eq!(c["support"], "supported", "{f}: {m}");
        assert_eq!(c["source"], "observed");
        assert!(c["verified_on"].as_str().unwrap().len() == 10);
    }
    assert_eq!(m["current_capabilities"]["tools"]["support"], "unknown");
    assert_eq!(
        s.json(&["assist", "status"])["capabilities"]["streaming"]["support"],
        "supported"
    );
    // The test is also visible as metadata-only events, never as content.
    let ev = s.events();
    let t = ev
        .iter()
        .find(|e| e["type"] == "assistant.test_finished")
        .unwrap();
    assert_eq!(t["data"]["ok"], true);
    assert!(!t.to_string().contains("Reply now"));
}

#[test]
fn a_provider_rejecting_a_probe_is_recorded_as_unsupported() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![
        Reply::anthropic("{\"ok\": true}", 10, 4),
        Reply::status(400, "{\"error\":\"tool_choice not allowed\"}"),
    ]);
    s.config(&fake.url(), "", "");
    let r = s.json(&["assist", "test", "--probe", "json_schema"]);
    assert_eq!(r["ok"], true);
    assert_eq!(r["probes"][0]["ok"], false);
    assert_eq!(r["probes"][0]["recorded"], "unsupported", "{r}");
    let m = s.json(&["assist", "models", "--connection", "primary"]);
    assert_eq!(
        m["current_capabilities"]["json_schema"]["support"],
        "unsupported"
    );
    assert_eq!(
        m["current_capabilities"]["json_schema"]["source"],
        "observed"
    );
    // A failing plain test reports the category, counts its attempt, and probes nothing.
    let fake2 = FakeServer::start_in_thread(vec![Reply::status(401, "{}")]);
    s.config(&fake2.url(), "", "");
    let before = s.used()["requests"].as_u64().unwrap();
    let r = s.json(&["assist", "test", "--probe", "streaming"]);
    assert_eq!(r["ok"], false);
    assert_eq!(r["error"]["category"], "authentication_failed");
    assert!(r["probes"].as_array().unwrap().is_empty());
    assert_eq!(s.used()["requests"].as_u64().unwrap(), before + 1);
}

// ---- streaming, native structured output, lifecycle events ----------------------------------------

#[test]
fn streaming_and_native_schema_are_used_only_when_supported_and_deltas_are_transient() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    s.config(
        &fake.url(),
        "",
        "capabilities = { streaming = \"supported\", json_schema = \"supported\" }",
    );
    let (ws, pane) = s.workspace();
    s.json(&["assist", "consent", &ws]);

    // Follow the transient deltas the way a client would.
    let mut child = s
        .cmd(&[
            "events",
            "tail",
            "--types",
            "assistant.delta",
            "--lines",
            "0",
            "--follow",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            let _ = tx.send(l);
        }
    });
    std::thread::sleep(Duration::from_millis(1500));

    fake.push(tool_stream(&["{\"ti", "tle\":", "\"Streamed\"}"], 40, 9));
    let g = s
        .api(
            "assistant.generate",
            json!({"operation": "pane_title", "pane": pane}),
        )
        .unwrap();
    let id = g["request"]["id"].as_str().unwrap().to_string();
    // The preview is the payload; the wire mode is decided at dispatch from capability records.
    assert_eq!(g["request"]["state"], "awaiting_confirmation");
    assert_eq!(fake.count(), 0);
    s.api(
        "assistant.confirm",
        json!({"request": id, "preview_digest": g["preview"]["digest"]}),
    )
    .unwrap();
    let done = s.wait_state(&id, &["done", "failed"]);
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["streamed"], true);
    assert_eq!(done["structured"], "native");
    assert_eq!(done["output"]["title"], "Streamed");
    assert_eq!(done["usage"]["output_tokens"], 9);
    let wire = fake.requests()[0].json();
    assert_eq!(wire["stream"], true);
    assert_eq!(wire["tools"][0]["name"], "emit_result");
    assert_eq!(wire["tool_choice"]["type"], "tool");
    assert_eq!(wire["messages"][0]["content"], g["preview"]["user"]);

    // Deltas arrived in order, as transient notifications with the request id and a sequence.
    let mut deltas: Vec<Value> = vec![];
    while let Ok(l) = rx.recv_timeout(Duration::from_millis(800)) {
        deltas.push(serde_json::from_str(&l).unwrap());
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(deltas.len() >= 3, "{deltas:?}");
    let mut text = String::new();
    let mut last = 0;
    for d in &deltas {
        assert_eq!(d["type"], "assistant.delta");
        assert_eq!(d["data"]["request"], id);
        let seq = d["data"]["seq"].as_u64().unwrap();
        assert!(seq > last);
        last = seq;
        text.push_str(d["data"]["text"].as_str().unwrap());
    }
    assert_eq!(text, "{\"title\":\"Streamed\"}");
    // They are not history: the durable log has the lifecycle (metadata, system actor) only.
    let ev = s.events();
    assert!(ev.iter().all(|e| e["type"] != "assistant.delta"));
    for kind in [
        "assistant.request_created",
        "assistant.request_started",
        "assistant.request_finished",
    ] {
        let e = ev
            .iter()
            .find(|e| e["type"] == kind)
            .unwrap_or_else(|| panic!("{kind}"));
        assert_eq!(e["actor"]["kind"], "system", "{kind}");
        assert!(
            !e.to_string().contains("Streamed"),
            "{kind} carries no generated text"
        );
    }
    // Without a declared or observed capability the same request is one plain JSON reply.
    s.config(&fake.url(), "", "");
    s.json(&["assist", "purge", "--all"]);
    let plain = FakeServer::start_in_thread(vec![Reply::anthropic(&title("Plain"), 12, 4)]);
    s.config(&plain.url(), "", "");
    s.json(&["assist", "consent", &ws]);
    let done = s.run(json!({"operation": "pane_title", "pane": pane}));
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["streamed"], false);
    assert_eq!(done["structured"], "json_text");
    let wire = plain.requests()[0].json();
    assert!(
        wire.get("stream").is_none() && wire.get("tools").is_none(),
        "{wire}"
    );
}

// ---- one bounded repair attempt (14 §8) -------------------------------------------------------------

#[test]
fn invalid_output_gets_exactly_one_repair_attempt_that_counts_against_the_limits() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![
        Reply::anthropic(r#"{"nope": 1}"#, 30, 5),
        Reply::anthropic(&title("Repaired"), 45, 6),
    ]);
    s.config(&fake.url(), "", "");
    let (ws, pane) = s.workspace();
    s.json(&["assist", "consent", &ws]);
    let done = s.run(json!({"operation": "pane_title", "pane": pane}));
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["output"]["title"], "Repaired");
    assert_eq!(done["repaired"], true);
    assert_eq!(done["attempts"], 2);
    assert_eq!(done["usage"]["input_tokens"], 75);
    assert_eq!(fake.count(), 2);
    let second = fake.requests()[1].json();
    let user = second["messages"][0]["content"].as_str().unwrap();
    assert!(
        user.contains("<previous_reply>") && user.contains("nope"),
        "{user}"
    );
    assert!(user.contains("rejected"), "{user}");
    assert_eq!(
        s.used()["requests"],
        2,
        "the repair counts toward the request limit"
    );

    // Invalid twice: the request fails and nothing is attempted a third time.
    let fake2 = FakeServer::start_in_thread(vec![
        Reply::anthropic(r#"{"nope": 1}"#, 10, 2),
        Reply::anthropic(r#"{"still": "nope"}"#, 10, 2),
        Reply::anthropic(&title("never sent"), 10, 2),
    ]);
    s.config(&fake2.url(), "", "");
    s.json(&["assist", "consent", &ws]);
    let failed = s.run(json!({"operation": "pane_title", "pane": pane}));
    assert_eq!(failed["state"], "failed", "{failed}");
    assert_eq!(failed["error"]["category"], "invalid_output");
    assert_eq!(failed["attempts"], 2);
    assert_eq!(fake2.count(), 2);

    // A truncated reply is never repaired.
    let fake3 = FakeServer::start_in_thread(vec![
        Reply::anthropic_stop("{\"title\": \"cut", "max_tokens"),
        Reply::anthropic(&title("never sent"), 10, 2),
    ]);
    s.config(&fake3.url(), "", "");
    s.json(&["assist", "consent", &ws]);
    let failed = s.run(json!({"operation": "pane_title", "pane": pane}));
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["error"]["category"], "invalid_output");
    assert_eq!(fake3.count(), 1);
}

// ---- result cache, access recheck, forget ------------------------------------------------------------

#[test]
fn cached_results_need_the_same_scope_grants_and_sources_and_forget_removes_them() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![briefing_ok()]);
    s.config(&fake.url(), "result_cache = true", "");
    let (ws, _pane) = s.workspace();
    s.json(&["assist", "consent", &ws]);

    let first = s.run(json!({"operation": "briefing", "workspace": ws}));
    assert_eq!(first["state"], "done", "{first}");
    assert_eq!(first["cached"], false);
    assert_eq!(fake.count(), 1);
    let first_id = first["id"].as_str().unwrap().to_string();

    // The same context under the same grants: answered from the cache, nothing sent, no usage.
    let hit = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap();
    assert_eq!(hit["cached"], true, "{hit}");
    assert_eq!(hit["requires_confirmation"], false);
    assert_eq!(hit["request"]["state"], "done");
    assert_eq!(hit["request"]["cached"], true);
    assert_eq!(hit["request"]["cache_origin"], first_id);
    assert_eq!(hit["request"]["output"]["generated"], true);
    assert_eq!(hit["request"]["usage"]["input_tokens"], Value::Null);
    assert_eq!(fake.count(), 1);
    assert_eq!(s.used()["requests"], 1);

    // Granting again changes the grants in force: no hit, a preview instead.
    s.json(&["assist", "consent", &ws]);
    let miss = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap();
    assert_eq!(miss["requires_confirmation"], true, "{miss}");
    assert!(miss.get("cached").is_none() || miss["cached"] != true);
    s.json(&["assist", "cancel", miss["request"]["id"].as_str().unwrap()]);

    // A stored result is re-authorized whenever it is read.
    assert!(s.get(&first_id).get("output").is_some_and(|o| !o.is_null()));
    s.json(&["assist", "revoke", &ws]);
    let withheld = s.get(&first_id);
    assert_eq!(withheld["output_withheld"], true, "{withheld}");
    assert_eq!(withheld["access"], "revoked");
    assert!(withheld.get("output").is_none());
    s.json(&["assist", "consent", &ws]);
    assert!(s.get(&first_id).get("output").is_some_and(|o| !o.is_null()));

    // `forget` for the workspace removes the derived records and cached results.
    let hit = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap();
    assert_ne!(hit["cached"], true, "the re-grant changed the key");
    s.json(&["assist", "cancel", hit["request"]["id"].as_str().unwrap()]);
    let fake_b = FakeServer::start_in_thread(vec![briefing_ok()]);
    s.config(&fake_b.url(), "result_cache = true", "");
    s.json(&["assist", "consent", &ws]);
    let a = s.run(json!({"operation": "briefing", "workspace": ws}));
    assert_eq!(a["state"], "done");
    let cached = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap();
    assert_eq!(cached["cached"], true);
    s.api("scrollback.forget", json!({"workspace": ws}))
        .unwrap();
    assert!(
        s.json(&["assist", "list"])["requests"]
            .as_array()
            .unwrap()
            .is_empty(),
        "forgetting the workspace removed the assistant's derived records"
    );
    let after = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap();
    assert_eq!(
        after["requires_confirmation"], true,
        "the cached result went too: {after}"
    );
    assert!(
        s.events()
            .iter()
            .any(|e| e["type"] == "assistant.purged" && e["data"]["reason"] == "forget")
    );
}

// ---- priority and coalescing, the background opt-in -------------------------------------------------

#[test]
fn background_work_is_opt_in_and_coalesced_per_scope() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    s.config(&fake.url(), "", "");
    let (ws, _pane) = s.workspace();
    s.json(&["assist", "consent", &ws]);
    // Off by default: a background request, and the background-only operations, are refused.
    let e = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws, "priority": "background"}),
        )
        .unwrap_err();
    assert_eq!(category(&e), "disabled", "{e}");
    let e = s
        .api(
            "assistant.generate",
            json!({"operation": "background_summary", "workspace": ws}),
        )
        .unwrap_err();
    assert_eq!(reason(&e), "background_disabled", "{e}");
    let st = s.json(&["assist", "status"]);
    assert_eq!(st["background"], false);
    // The master switch alone is not enough for the summary feature.
    s.config(&fake.url(), "background_enabled = true", "");
    let e = s
        .api(
            "assistant.generate",
            json!({"operation": "background_summary", "workspace": ws}),
        )
        .unwrap_err();
    assert_eq!(reason(&e), "background_disabled", "{e}");

    // Two clients asking for the same background work share one request.
    let a = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws, "priority": "background"}),
        )
        .unwrap();
    assert_eq!(a["request"]["priority"], "background");
    let id = a["request"]["id"].as_str().unwrap().to_string();
    let b = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws, "priority": "background"}),
        )
        .unwrap();
    assert_eq!(b["coalesced"], true, "{b}");
    assert_eq!(b["request"]["id"], id);
    assert_eq!(s.get(&id)["consumer_count"], 1);
    // The attached client can only detach; the creator's request keeps going.
    let d = s.api("assistant.cancel", json!({"request": id})).unwrap();
    assert_eq!(d["detached"], true, "{d}");
    assert_eq!(s.get(&id)["state"], "awaiting_confirmation");
    assert_eq!(fake.count(), 0);
    // Interactive requests are never coalesced.
    let i1 = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap();
    assert_ne!(i1["request"]["id"], id);
    assert_eq!(i1["request"]["priority"], "interactive");
}

// ---- semantic navigation (A2) -----------------------------------------------------------------------

#[test]
fn navigation_ranks_authorized_candidates_and_only_cites_what_was_offered() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    s.config(&fake.url(), "", "");
    let (ws, pane) = s.workspace();
    s.json(&["assist", "consent", &ws]);
    let (run, _) = s.agent(
        &pane,
        "fake-claude",
        FAKE_CLAUDE,
        "sess-nav",
        "fix the login redirect bug",
    );

    let g = s
        .api(
            "assistant.generate",
            json!({"operation": "navigate", "workspace": ws, "query": "where is the login redirect work"}),
        )
        .unwrap();
    let user = g["preview"]["user"].as_str().unwrap();
    assert!(user.contains("kind=\"query\""), "{user}");
    assert!(user.contains("kind=\"candidate\""), "{user}");
    assert!(user.contains("fix the login redirect bug"), "{user}");
    assert!(user.contains(&format!("Valid target ids: {run}")), "{user}");
    let id = g["request"]["id"].as_str().unwrap().to_string();
    fake.push(Reply::anthropic(
        &json!({"matches": [{"target": run, "kind": "task", "reason": "working on the login redirect", "confidence": "high", "source_refs": ["s2"], "method": "pane.focus"}], "coverage": "1 candidate"}).to_string(),
        90,
        25,
    ));
    s.api(
        "assistant.confirm",
        json!({"request": id, "preview_digest": g["preview"]["digest"]}),
    )
    .unwrap();
    let done = s.wait_state(&id, &["done", "failed"]);
    assert_eq!(done["state"], "done", "{done}");
    let m = &done["output"]["matches"][0];
    assert_eq!(m["target"], run);
    assert_eq!(
        m["kind"], "run",
        "Vibeke's own kind replaces the model's guess"
    );
    assert_eq!(m["open"]["id"], run);
    assert!(m.get("method").is_none(), "nothing the model adds survives");
    assert_eq!(done["stale"], false);
    let live = &done["live_targets"][0];
    assert_eq!(live["id"], run);
    assert_eq!(live["exists"], true);
    assert_eq!(live["kind"], "run");

    // An id that was never offered fails validation (and its one repair), never a result.
    fake.push(Reply::anthropic(
        r#"{"matches":[{"target":"r999","reason":"x"}]}"#,
        10,
        5,
    ));
    fake.push(Reply::anthropic(
        r#"{"matches":[{"target":"r999","reason":"x"}]}"#,
        10,
        5,
    ));
    let before = fake.count();
    let failed = s.run(json!({"operation": "navigate", "workspace": ws, "query": "anything"}));
    assert_eq!(failed["state"], "failed", "{failed}");
    assert_eq!(failed["error"]["category"], "invalid_output");
    assert_eq!(fake.count(), before + 2);
    // A query is required, and another workspace's objects are never offered.
    let e = s
        .api(
            "assistant.generate",
            json!({"operation": "navigate", "workspace": ws}),
        )
        .unwrap_err();
    assert!(e.to_string().contains("--query"), "{e}");
    let (ws2, _p2) = s.workspace();
    s.json(&["assist", "consent", &ws2]);
    let g2 = s
        .api(
            "assistant.generate",
            json!({"operation": "navigate", "workspace": ws2, "query": "login redirect"}),
        )
        .unwrap();
    assert!(
        !g2["preview"]["user"]
            .as_str()
            .unwrap()
            .contains("fix the login redirect bug"),
        "candidates come only from the consented workspace asked about"
    );
}

#[test]
fn task_titles_are_one_line_suggestions_that_are_never_applied() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![Reply::anthropic(
        &json!({"title": "Fix login redirect\nsecond line", "rationale": "from the request", "method": "pane.rename"}).to_string(),
        30,
        10,
    )]);
    s.config(&fake.url(), "", "");
    let (ws, pane) = s.workspace();
    s.json(&["assist", "consent", &ws]);
    let (run, _) = s.agent(
        &pane,
        "fake-claude",
        FAKE_CLAUDE,
        "sess-title",
        "fix the login redirect bug",
    );
    let done = s.run(json!({"operation": "task_title", "run": run}));
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["output"]["title"], "Fix login redirect");
    assert_eq!(done["output"]["applied"], false);
    assert_eq!(done["output"]["preserves_user_title"], true);
    assert!(done["output"].get("method").is_none());
    let e = s
        .api(
            "assistant.generate",
            json!({"operation": "task_title", "workspace": ws}),
        )
        .unwrap_err();
    assert!(e.to_string().contains("--task or --run"), "{e}");
}

// ---- decision cards and staleness (A2, 14 §7.2) ------------------------------------------------------

#[test]
fn decision_cards_are_editable_drafts_and_go_stale_when_the_interaction_resolves() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    s.config(&fake.url(), "", "");
    let (ws, pane) = s.workspace();
    s.json(&["assist", "consent", &ws]);
    let go = s.dir.path().join("go");
    let body = format!(
        r#"#!/bin/sh
h() {{ printf '%s' "$2" | "$VIBEKE_BIN" hook gemini "$1" >/dev/null 2>&1; }}
h SessionStart '{{"session_id":"gem-1","source":"startup","cwd":"/tmp"}}'
while true; do
  printf '\n> Type your message\n'
  IFS= read -r line || exit 0
  h BeforeAgent "{{\"session_id\":\"gem-1\",\"prompt\":\"$line\"}}"
  h BeforeTool '{{"session_id":"gem-1","tool_name":"run_shell_command","tool_input":{{"command":"rm -rf dist"}}}}'
  h Notification '{{"session_id":"gem-1","notification_type":"ToolPermission","message":"Allow execution of: rm?","details":{{"command":"rm -rf dist"}}}}'
  while [ ! -f "{go}" ]; do sleep 0.1; done; rm -f "{go}"
  h AfterTool '{{"session_id":"gem-1","tool_name":"run_shell_command","tool_input":{{"command":"rm -rf dist"}},"tool_response":{{"exitCode":0}}}}'
  h AfterAgent '{{"session_id":"gem-1","prompt_response":"Cleaned up dist."}}'
done
"#,
        go = go.display()
    );
    let script = s.script("fake-gemini", &body);
    s.quiet(&pane);
    s.json(&["pane", "run", &pane, &script.to_string_lossy()]);
    s.until("gemini run", 15, || {
        let runs = s.json(&["agent", "list"]);
        runs["runs"]
            .as_array()?
            .iter()
            .find(|r| {
                (r["pane"] == pane.as_str() || r["pane_handle"] == pane.as_str())
                    && r["harness"] == "gemini"
            })
            .map(|_| ())
    });
    s.json(&["pane", "run", &pane, "clean up the build"]);
    let it = s.until("open interaction", 15, || {
        let v = s.api("interaction.list", json!({})).ok()?;
        v["interactions"]
            .as_array()?
            .iter()
            .find(|i| i["pane"] == pane.as_str() && i["status"] == "Open")
            .cloned()
    });
    let iid = it["id"].as_str().unwrap().to_string();

    let g = s
        .api(
            "assistant.generate",
            json!({"operation": "decision_card", "interaction": iid}),
        )
        .unwrap();
    let user = g["preview"]["user"].as_str().unwrap();
    assert!(user.contains("rm -rf dist"), "{user}");
    assert!(user.contains(&format!("Valid target ids: {iid}")), "{user}");
    assert!(user.contains("kind=\"interaction\""), "{user}");
    let id = g["request"]["id"].as_str().unwrap().to_string();
    fake.push(Reply::anthropic(
        &json!({
            "explanation": "The agent wants to delete the dist directory.",
            "earlier_decisions": [],
            "reply_draft": {"decision": "none", "text": "Not yet: confirm the path first.", "method": "interaction.answer"},
            "cautions": ["destructive command"],
            "params": {"interaction": iid, "decision": "allow"},
        })
        .to_string(),
        80,
        30,
    ));
    s.api(
        "assistant.confirm",
        json!({"request": id, "preview_digest": g["preview"]["digest"]}),
    )
    .unwrap();
    let done = s.wait_state(&id, &["done", "failed"]);
    assert_eq!(done["state"], "done", "{done}");
    let out = &done["output"];
    assert_eq!(out["interaction"], iid);
    assert_eq!(out["draft_only"], true);
    assert_eq!(out["send_with"]["method"], "interaction.answer");
    assert_eq!(
        out["reply_draft"]["text"],
        "Not yet: confirm the path first."
    );
    assert!(out.get("params").is_none() && out["reply_draft"].get("method").is_none());
    // Generating the card never answered anything.
    let now = s
        .api("interaction.get", json!({"interaction": iid}))
        .unwrap();
    assert_eq!(now["interaction"]["status"], "Open");
    assert_eq!(done["stale"], false);
    assert_eq!(done["live_targets"][0]["status"], "open");

    // The agent finishes: the live interaction resolved, so the card is marked stale, and its
    // live target now says so (cached statuses are never authoritative).
    std::fs::write(&go, "").unwrap();
    let stale = s.until("stale card", 20, || {
        let r = s.get(&id);
        (r["stale"] == true).then_some(r)
    });
    assert!(
        stale["stale_targets"][0]
            .as_str()
            .unwrap()
            .contains("changed"),
        "{stale}"
    );
    assert_ne!(stale["live_targets"][0]["status"], "open");
    assert!(stale["refresh_hint"].is_string());
    // A card for an interaction that is not open is refused.
    let e = s
        .api(
            "assistant.generate",
            json!({"operation": "decision_card", "interaction": iid}),
        )
        .unwrap_err();
    assert_eq!(reason(&e), "interaction_not_open", "{e}");
}

// ---- the opt-in background sweeper (A3) --------------------------------------------------------------

#[test]
fn the_sweeper_summarizes_changes_and_announces_stalls_only_with_every_opt_in() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let top = |extra: &str| {
        format!(
            "background_enabled = true\nbackground_interval_seconds = 3600\nstall_repeat_threshold = 3\nauto_send = [\"background_summary\", \"stall_notice\"]\n{extra}"
        )
    };
    s.config(&fake.url(), &top("background_summaries = true"), "");
    let (ws, pane) = s.workspace();
    // Consent without auto_send for the background operations.
    s.json(&["assist", "consent", &ws]);
    let (run, _) = s.agent(
        &pane,
        "fake-claude-failing",
        FAKE_CLAUDE_FAILING,
        "sess-fail",
        "install the dependencies",
    );
    s.until("three recorded failing commands", 15, || {
        let v = s.api("task.sources", json!({"run": run})).ok()?;
        (v["turns"].as_array()?.len() == 1).then_some(())
    });

    // Both lists are required: without the workspace's own auto_send nothing is created.
    let t = s
        .api("assistant.background", json!({"action": "tick"}))
        .unwrap();
    assert_eq!(t["active"], true, "{t}");
    let a = t["actions"].as_array().unwrap();
    assert_eq!(a.len(), 1, "{t}");
    assert_eq!(a[0]["action"], "summary");
    assert!(
        a[0]["skipped"]
            .as_str()
            .unwrap()
            .starts_with("auto_send_not_granted"),
        "{t}"
    );
    assert_eq!(fake.count(), 0);

    // Granted: one coalesced summary for the changed workspace.
    s.json(&[
        "assist",
        "consent",
        &ws,
        "--auto-send",
        "background_summary,stall_notice",
    ]);
    fake.push(Reply::anthropic(
        &json!({"items": [{"text": "One agent is running.", "kind": "observed", "urgency": "fyi", "targets": [], "source_refs": ["s1"]}], "coverage": "one workspace"}).to_string(),
        70,
        20,
    ));
    let t = s
        .api("assistant.background", json!({"action": "tick"}))
        .unwrap();
    let a = t["actions"].as_array().unwrap();
    assert_eq!(a.len(), 1, "{t}");
    let sid = a[0]["request"].as_str().unwrap().to_string();
    let done = s.wait_state(&sid, &["done", "failed"]);
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["operation"], "background_summary");
    assert_eq!(done["priority"], "background");
    assert_eq!(done["auto_sent"], true);
    assert_eq!(fake.count(), 1);
    // Unchanged state: nothing more, however many sweeps.
    let t = s
        .api("assistant.background", json!({"action": "tick"}))
        .unwrap();
    assert!(t["actions"].as_array().unwrap().is_empty(), "{t}");
    assert_eq!(fake.count(), 1);

    // Stall notices are a second opt-in on top; with it, the repeated failure is announced once.
    s.config(
        &fake.url(),
        &top("background_summaries = true\nstall_notices = true"),
        "",
    );
    fake.push(Reply::anthropic(
        &json!({"stalled": true, "summary": "npm install failed three times in a row", "evidence": [{"text": "same command, same exit code", "source_refs": ["s1"]}], "suggestion": "check registry authentication"}).to_string(),
        60,
        25,
    ));
    let t = s
        .api("assistant.background", json!({"action": "tick"}))
        .unwrap();
    let a = t["actions"].as_array().unwrap();
    let stall = a
        .iter()
        .find(|x| x["action"] == "stall")
        .unwrap_or_else(|| panic!("no stall action: {t}"));
    assert_eq!(stall["subject"], run);
    let rid = stall["request"].as_str().unwrap().to_string();
    let done = s.wait_state(&rid, &["done", "failed"]);
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["output"]["stalled"], true);
    assert_eq!(done["output"]["applied"], false);
    let n = s.until("the passive notification", 10, || {
        let v = s.api("notification.list", json!({})).ok()?;
        v["notifications"]
            .as_array()?
            .iter()
            .find(|n| n["kind"] == "assistant")
            .cloned()
    });
    assert!(n["title"].as_str().unwrap().contains("Possible stall"));
    assert!(n["body"].as_str().unwrap().contains("generated"));
    assert!(
        s.events()
            .iter()
            .any(|e| e["type"] == "assistant.stall_notice")
    );
    // The same loop is not announced again.
    let t = s
        .api("assistant.background", json!({"action": "tick"}))
        .unwrap();
    assert!(
        t["actions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["action"] != "stall"),
        "{t}"
    );
    // Nothing was ever answered, sent or changed by any of it.
    let st = s
        .api("assistant.background", json!({"action": "status"}))
        .unwrap();
    assert_eq!(st["active"], true);
    assert_eq!(st["stall_notices"], true);
}

// ---- sources from other machines (14 §4, §7.1) --------------------------------------------------------

#[test]
fn remote_sources_need_their_own_consent_and_state_their_coverage() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    let (ws, _pane) = s.workspace();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let remote = |cursor: &str, from: &str, observed: i64| {
        json!([
            {"machine": "devbox", "session": "main", "workspace": "/home/u/repo", "status": "ok",
             "cursor": cursor, "from_cursor": from, "observed_at_ms": observed,
             "items": [{"kind": "run_state", "object": {"run": "r1"}, "label": "remote agent", "text": "running the test suite"}]},
            {"machine": "lab", "session": "main", "workspace": "/srv/x", "status": "offline"},
        ])
    };
    let gen_with = |remote: Value| {
        s.api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws, "remote_sources": remote}),
        )
    };
    // Off by default.
    s.config(&fake.url(), "", "");
    s.json(&["assist", "consent", &ws]);
    let e = gen_with(remote("10", "0", now)).unwrap_err();
    assert_eq!(reason(&e), "remote_sources_off", "{e}");

    s.config(&fake.url(), "remote_sources = true", "");
    s.json(&["assist", "consent", &ws]);
    // A local grant never covers a remote workspace.
    let e = gen_with(remote("10", "0", now)).unwrap_err();
    assert_eq!(reason(&e), "consent_required", "{e}");
    assert!(e.to_string().contains("devbox:/home/u/repo"), "{e}");
    let c = s.json(&[
        "assist",
        "consent",
        "--remote-workspace",
        "devbox:/home/u/repo",
    ]);
    assert_eq!(c["consent"]["workspace"], "devbox:/home/u/repo");

    let g = gen_with(remote("10", "0", now)).unwrap();
    let user = g["preview"]["user"].as_str().unwrap();
    assert!(user.contains("remote agent [devbox/main]"), "{user}");
    assert!(user.contains("running the test suite"), "{user}");
    assert!(
        !user.contains("[lab/main"),
        "an offline source contributes no content"
    );
    let notes = g["preview"]["coverage_notes"].as_array().unwrap();
    let all: Vec<&str> = notes.iter().filter_map(Value::as_str).collect();
    assert!(
        all.iter().any(|n| n.contains("lab/main is offline")),
        "{all:?}"
    );
    assert!(
        all.iter().any(|n| n.contains("devbox/main included")),
        "{all:?}"
    );
    assert!(user.contains("Coverage notes"), "{user}");
    let remote_src = g["request"]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| {
            x["kind"] == "run_state"
                && x["identity"]
                    .as_str()
                    .is_some_and(|i| i.starts_with("devbox/main/"))
        })
        .unwrap_or_else(|| panic!("{g}"))
        .clone();
    assert_eq!(remote_src["identity"], "devbox/main/run_state:run=r1");
    assert_eq!(remote_src["cursor"], "10");
    // A remote request never auto-sends and needs its own confirmation.
    assert_eq!(g["requires_confirmation"], true);
    assert!(
        g["request"]["other_workspace_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "devbox:/home/u/repo")
    );
    s.json(&["assist", "cancel", g["request"]["id"].as_str().unwrap()]);

    // History between the last cursor and where the items start is reported as missing, and
    // an old observation is labelled stale.
    let g = gen_with(remote("20", "14", 0)).unwrap();
    let notes: Vec<String> = g["preview"]["coverage_notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n.as_str().map(str::to_string))
        .collect();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("between cursor 10 and 14 are missing")
                && n.contains("not a complete account")),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(|n| n.contains("last observed")),
        "{notes:?}"
    );
    assert!(g["preview"]["user"].as_str().unwrap().contains(", stale]"));
    // Revoking the remote grant cancels the pending request and refuses new ones.
    let r = s.json(&[
        "assist",
        "revoke",
        "--remote-workspace",
        "devbox:/home/u/repo",
    ]);
    assert_eq!(r["revoked"], 1);
    assert_eq!(r["cancelled_requests"], 1, "{r}");
    let e = gen_with(remote("30", "20", now)).unwrap_err();
    assert_eq!(reason(&e), "consent_required");
    assert_eq!(fake.count(), 0, "nothing reached the provider");
    // Remote sources with a request that doesn't take them are refused.
    let e = s
        .api(
            "assistant.generate",
            json!({"operation": "pane_title", "workspace": ws, "remote_sources": []}),
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("--pane") || e.to_string().contains("does not take"),
        "{e}"
    );
}

// ---- adapters and credentials ---------------------------------------------------------------------------

#[test]
fn the_gemini_adapter_speaks_the_native_api() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![Reply::gemini(
        &json!({"items": [{"text": "Nothing needs you.", "kind": "observed", "urgency": "fyi", "targets": [], "source_refs": ["s1"]}], "coverage": "c"}).to_string(),
        33,
        11,
    )]);
    s.write_config(&format!(
        r#"
[assistant]
enabled = true

[assistant.connections.g]
adapter = "gemini"
endpoint = "{}"
credential = {{ env = "VK_TEST_ASSIST_KEY" }}

[assistant.profiles.interactive]
connection = "g"
model = "gemini-test"
"#,
        fake.url()
    ));
    let (ws, _pane) = s.workspace();
    s.json(&["assist", "consent", &ws]);
    let done = s.run(json!({"operation": "briefing", "workspace": ws}));
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(done["adapter"], "gemini");
    assert_eq!(done["usage"]["input_tokens"], 33);
    let sent = &fake.requests()[0];
    assert_eq!(sent.path, "/v1beta/models/gemini-test:generateContent");
    assert_eq!(sent.header("x-goog-api-key").as_deref(), Some(KEY));
    assert!(sent.header("authorization").is_none());
    assert!(sent.json()["systemInstruction"]["parts"][0]["text"].is_string());
    let targets = s.json(&["assist", "providers"])["targets"].clone();
    for name in [
        "anthropic",
        "openai",
        "gemini",
        "openrouter",
        "ollama",
        "custom",
    ] {
        assert!(
            targets
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == name),
            "{name}"
        );
    }
}

#[test]
fn keychain_credentials_resolve_through_the_selected_backend_without_fallback() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![briefing_ok()]);
    let kc = s.dir.path().join("fake-keychain.json");
    std::fs::write(&kc, r#"{"vibeke/assistant/primary": "sk-from-keychain"}"#).unwrap();
    std::fs::set_permissions(&kc, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let conf = |backend: &str| {
        format!(
            r#"
[assistant]
enabled = true
keychain_backend = "{backend}"

[assistant.connections.primary]
adapter = "anthropic"
endpoint = "{}"
credential = {{ keychain = "vibeke/assistant/primary" }}

[assistant.profiles.interactive]
connection = "primary"
model = "claude-haiku-4-5-20251001"
"#,
            fake.url()
        )
    };
    let (ws, _pane) = s.workspace();
    // Off (the default): the reference is reported unsupported and nothing is sent, even
    // though an ambient env credential exists.
    s.write_config(&conf("off"));
    s.json(&["assist", "consent", &ws]);
    let failed = s.run(json!({"operation": "briefing", "workspace": ws}));
    assert_eq!(failed["state"], "failed", "{failed}");
    assert_eq!(failed["error"]["category"], "unsupported_capability");
    assert_eq!(fake.count(), 0);
    // The fake backend: the key comes from the keychain item and is what the provider sees.
    s.write_config(&conf("fake"));
    s.json(&["assist", "consent", &ws]);
    let done = s.run(json!({"operation": "briefing", "workspace": ws}));
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(
        fake.requests()[0].header("x-api-key").as_deref(),
        Some("sk-from-keychain")
    );
    assert!(!done.to_string().contains("sk-from-keychain"));
    // A missing item is an authentication failure, never a silent fallback.
    std::fs::write(&kc, r#"{"other": "x"}"#).unwrap();
    std::fs::set_permissions(&kc, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    let failed = s.run(json!({"operation": "briefing", "workspace": ws}));
    assert_eq!(
        failed["error"]["category"], "authentication_failed",
        "{failed}"
    );
    assert_eq!(fake.count(), 1);
}

// ---- disable while in flight, live concurrency changes, switching -----------------------------------------

#[test]
fn disabling_aborts_an_in_flight_request_and_concurrency_changes_apply_live() {
    let s = Session::new();
    let slow = |n: usize| {
        (0..n)
            .map(|_| briefing_ok().delayed(Duration::from_secs(5)))
            .collect::<Vec<_>>()
    };
    let fake = FakeServer::start_in_thread(slow(1));
    s.config(&fake.url(), "", "");
    let (ws, _pane) = s.workspace();
    s.json(&["assist", "consent", &ws]);
    let g = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap();
    let id = g["request"]["id"].as_str().unwrap().to_string();
    s.api(
        "assistant.confirm",
        json!({"request": id, "preview_digest": g["preview"]["digest"]}),
    )
    .unwrap();
    s.until("the provider has the request", 10, || {
        (fake.count() == 1).then_some(())
    });
    // Disabled in config while the request is in flight: it is aborted without a poll from
    // anyone, and its reservation is charged (it may have been billed).
    s.write_config(&cfg(false, &fake.url(), "", ""));
    let r = s.wait_state(&id, &["cancelled", "done", "failed"]);
    assert_eq!(r["state"], "cancelled", "{r}");
    assert_eq!(r["error"]["category"], "disabled");
    assert_eq!(fake.count(), 1);
    assert!(s.used()["requests"].as_u64().unwrap() >= 1);

    // One slot: the second request waits. Raising the limit applies at the next dispatch,
    // without a restart, and admits the waiting request.
    let fake = FakeServer::start_in_thread(slow(3));
    s.config(
        &fake.url(),
        "max_concurrent_requests = 1\nrequests_per_minute = 30",
        "",
    );
    s.json(&["assist", "consent", &ws]);
    let mut ids = vec![];
    for _ in 0..2 {
        let g = s
            .api(
                "assistant.generate",
                json!({"operation": "briefing", "workspace": ws}),
            )
            .unwrap();
        let id = g["request"]["id"].as_str().unwrap().to_string();
        s.api(
            "assistant.confirm",
            json!({"request": id, "preview_digest": g["preview"]["digest"]}),
        )
        .unwrap();
        ids.push(id);
    }
    s.until("the first request is in flight", 10, || {
        (fake.count() >= 1).then_some(())
    });
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        fake.count(),
        1,
        "the second request waits for the only slot"
    );
    s.config(
        &fake.url(),
        "max_concurrent_requests = 2\nrequests_per_minute = 30",
        "",
    );
    let g = s
        .api(
            "assistant.generate",
            json!({"operation": "briefing", "workspace": ws}),
        )
        .unwrap();
    s.api(
        "assistant.confirm",
        json!({"request": g["request"]["id"], "preview_digest": g["preview"]["digest"]}),
    )
    .unwrap();
    s.until("waiting requests admitted by the new limit", 3, || {
        (fake.count() == 3).then_some(())
    });
    for id in &ids {
        s.json(&["assist", "cancel", id]);
    }
}

#[test]
fn a_user_can_switch_between_a_cloud_and_a_local_connection_without_restarting_the_pane() {
    let s = Session::new();
    let cloud = FakeServer::start_in_thread(vec![Reply::anthropic(&title("Cloud title"), 10, 4)]);
    let local = FakeServer::start_in_thread(vec![Reply::ollama(&title("Local title"), 12, 5)]);
    s.write_config(&format!(
        r#"
[assistant]
enabled = true

[assistant.connections.primary]
adapter = "anthropic"
endpoint = "{}"
credential = {{ env = "VK_TEST_ASSIST_KEY" }}

[assistant.connections.local]
adapter = "ollama"
endpoint = "{}"

[assistant.profiles.interactive]
connection = "primary"
model = "claude-haiku-4-5-20251001"

[assistant.profiles.local]
connection = "local"
model = "llama3"
"#,
        cloud.url(),
        local.url()
    ));
    let (ws, pane) = s.workspace();
    s.json(&["assist", "consent", &ws, "--connection", "primary"]);
    s.json(&["assist", "consent", &ws, "--connection", "local"]);
    let (run, _) = s.agent(
        &pane,
        "fake-claude",
        FAKE_CLAUDE,
        "sess-switch",
        "refactor the parser",
    );
    let pid_before = s.api("pane.get", json!({"pane": pane})).unwrap()["pane"]["child_pid"].clone();

    let a = s.run(json!({"operation": "pane_title", "pane": pane}));
    assert_eq!(a["state"], "done", "{a}");
    assert_eq!(a["connection"], "primary");
    assert_eq!(a["output"]["title"], "Cloud title");
    let b = s.run(json!({"operation": "pane_title", "pane": pane, "profile": "local"}));
    assert_eq!(b["state"], "done", "{b}");
    assert_eq!(b["connection"], "local");
    assert_eq!(b["output"]["title"], "Local title");
    assert_eq!(cloud.count(), 1);
    assert_eq!(local.count(), 1);
    assert_eq!(local.requests()[0].path, "/api/chat");
    // The hosted agent's run and process are untouched by either switch.
    let runs = s.json(&["agent", "list"]);
    let r = runs["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == run.as_str())
        .unwrap_or_else(|| panic!("{runs}"));
    assert!(r["ended_at_ms"].is_null());
    assert_eq!(
        s.api("pane.get", json!({"pane": pane})).unwrap()["pane"]["child_pid"],
        pid_before
    );
}

// ---- doctor ----------------------------------------------------------------------------------------------

#[test]
fn doctor_diagnoses_assistant_settings_without_a_provider_call() {
    let s = Session::new();
    let fake = FakeServer::start_in_thread(vec![]);
    s.write_config(&format!(
        r#"
[assistant]
enabled = true
background_summaries = true

[assistant.connections.primary]
adapter = "anthropic"
endpoint = "{}"
credential = {{ env = "VK_DOCTOR_MISSING_KEY" }}

[assistant.profiles.interactive]
connection = "primary"
model = "claude-haiku-4-5-20251001"
"#,
        fake.url()
    ));
    let out = s.cmd(&["doctor", "--no-remote"]).output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    let checks: Vec<&Value> = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["section"] == "assistant")
        .collect();
    assert!(!checks.is_empty(), "{v}");
    let find = |level: &str, needle: &str| {
        checks
            .iter()
            .any(|c| c["level"] == level && c["message"].as_str().unwrap().contains(needle))
    };
    assert!(
        find("fail", "VK_DOCTOR_MISSING_KEY is not set"),
        "{checks:?}"
    );
    assert!(find("warn", "have no effect"), "{checks:?}");
    assert!(find("info", "requests run on this machine"));
    assert_eq!(v["ok"], false, "a failing assistant check fails the report");
    assert_eq!(fake.count(), 0, "doctor never contacts a provider");
    assert_eq!(
        s.used()["requests"],
        0,
        "and never spends the request allowance"
    );
    // With the variable set the credential check passes and the secret is not in the report.
    let out = s
        .cmd(&["doctor", "--no-remote"])
        .env("VK_DOCTOR_MISSING_KEY", "sk-doctor-secret")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("VK_DOCTOR_MISSING_KEY is set"), "{text}");
    assert!(!text.contains("sk-doctor-secret"));
}

// ---- latency with concurrent slow streams (A1 acceptance 6) -----------------------------------------------------

#[test]
fn concurrent_slow_streams_do_not_slow_the_server_down() {
    let s = Session::new();
    let slow = || {
        Reply::anthropic_stream(&["{\"title\":", " \"", "Slow", "\"}"], 20, 10)
            .with_piece_delay(Duration::from_millis(500))
    };
    let fake = FakeServer::start_in_thread((0..4).map(|_| slow()).collect());
    s.config(
        &fake.url(),
        "max_concurrent_requests = 4\nrequests_per_minute = 60",
        "capabilities = { streaming = \"supported\" }",
    );
    let (ws, pane) = s.workspace();
    s.json(&["assist", "consent", &ws]);
    s.quiet(&pane);
    // A cheap API round trip through the CLI; the measurement includes process start, so it is
    // compared with the same measurement taken while the server is idle.
    let probe = || {
        let t = Instant::now();
        let out = s.cmd(&["api", "call", "pane.list", "{}"]).output().unwrap();
        assert!(out.status.success());
        t.elapsed()
    };
    let p95 = |mut v: Vec<Duration>| {
        v.sort();
        v[(v.len() * 95 / 100).min(v.len() - 1)]
    };
    let idle: Vec<Duration> = (0..20).map(|_| probe()).collect();
    let mut ids = vec![];
    for _ in 0..4 {
        let g = s
            .api(
                "assistant.generate",
                json!({"operation": "pane_title", "pane": pane}),
            )
            .unwrap();
        let id = g["request"]["id"].as_str().unwrap().to_string();
        s.api(
            "assistant.confirm",
            json!({"request": id, "preview_digest": g["preview"]["digest"]}),
        )
        .unwrap();
        ids.push(id);
    }
    s.until("all four streams open", 10, || {
        (fake.count() == 4).then_some(())
    });
    let loaded: Vec<Duration> = (0..20).map(|_| probe()).collect();
    for id in &ids {
        assert_eq!(s.wait_state(id, &["done", "failed"])["state"], "done");
    }
    let (i95, l95) = (p95(idle.clone()), p95(loaded.clone()));
    eprintln!(
        "assistant latency (CLI round trip incl. process start): idle p95 {i95:?}, with 4 concurrent slow streams p95 {l95:?}; idle max {:?}, loaded max {:?}",
        idle.iter().max().unwrap(),
        loaded.iter().max().unwrap()
    );
    // Asynchronous code is not assumed fast enough: the loaded round trips stay within a small
    // multiple of the idle ones (generous, the point is that they do not queue behind streams).
    assert!(
        l95 < i95 * 3 + Duration::from_millis(400),
        "loaded p95 {l95:?} vs idle p95 {i95:?}"
    );
}
