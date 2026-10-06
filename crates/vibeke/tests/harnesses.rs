//! M2 harnesses end to end through the real paths (server, pane, hook shim, ACP host) with fake
//! harnesses — never a live harness or a model:
//! - Gemini: fake CLI reporting through `vibeke hook gemini …`; a tool confirmation opens an
//!   observe-only approval that closes when the tool runs; turns/items are tracked.
//! - OpenCode: fake plugin calls through `vibeke hook opencode …`; with a user manifest asserting
//!   the approval capability, `permission.ask` is gated and answered natively.
//! - ACP: `agent.start --acp` launches a fake ACP agent (Python) through `vibeke acp-host`; the
//!   permission request is answered from Vibeke and reaches the agent as an ACP `optionId`.
//! - Herdr self-report, repo manifests behind `policy.trust`, `integration doctor` version gating.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Minimal PATH for the server and panes: no real harness binary can be found or run.
const SAFE_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

struct Session {
    dir: tempfile::TempDir,
    path: String,
}

impl Session {
    fn new() -> Self {
        Session {
            dir: tempfile::Builder::new()
                .prefix("vkharn")
                .tempdir_in("/tmp")
                .unwrap(),
            path: SAFE_PATH.to_string(),
        }
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env("PATH", &self.path)
            .env("HOME", d.join("home"));
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_RUNTIME_DIR",
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
    fn pane(&self, cwd: &Path) -> String {
        let pane =
            self.json(&["workspace", "create", "--cwd", &cwd.to_string_lossy()])["root_pane"]["id"]
                .as_str()
                .unwrap()
                .to_string();
        let _ = self
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
        pane
    }
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let p = self.dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        p
    }
    fn run_of(&self, pane: &str) -> Option<Value> {
        let runs = self.json(&["agent", "list"]);
        runs["runs"]
            .as_array()?
            .iter()
            .find(|r| r["pane"] == pane || r["pane_handle"] == pane)
            .cloned()
    }
    fn open_interaction(&self, pane: &str) -> Option<Value> {
        let v = self.api("interaction.list", json!({})).ok()?;
        v["interactions"]
            .as_array()?
            .iter()
            .find(|i| i["pane"] == pane && i["status"] == "Open")
            .cloned()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

#[test]
fn gemini_hooks_drive_state_interactions_and_tracking() {
    let s = Session::new();
    let go = s.dir.path().join("go");
    let fake = format!(
        r#"#!/bin/sh
# Fake Gemini CLI: reports through the real hook shim (payload shapes from spec 04 §6.5).
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
    let script = s.script("fake-gemini", &fake);
    let pane = s.pane(Path::new("/tmp"));
    s.json(&["pane", "run", &pane, &script.to_string_lossy()]);
    let run = s.until("gemini run bound by hooks", 15, || {
        s.run_of(&pane).filter(|r| r["harness"] == "gemini")
    });
    assert_eq!(run["integration"], "hooks");
    assert_eq!(
        run["capabilities"],
        json!(["observe", "answer_keystroke"]),
        "unverified harness: observe + keystrokes only"
    );
    s.json(&["pane", "run", &pane, "clean up the build"]);
    let it = s.until("tool confirmation", 15, || s.open_interaction(&pane));
    assert_eq!(it["kind"], "approval");
    assert_eq!(it["answer_channel"], "Keystrokes", "{it}");
    assert_eq!(
        it["gate"], false,
        "never gated: native answering unverified"
    );
    assert_eq!(it["action"]["command"], "rm -rf dist");
    assert_eq!(it["action"]["risk"], "High");
    let r = s.run_of(&pane).unwrap();
    assert_eq!(r["execution"]["value"], "Working");
    std::fs::write(&go, "").unwrap();
    s.until("turn finished", 15, || {
        let r = s.run_of(&pane)?;
        (r["execution"]["value"] == "Idle" && r["last_message"] == "Cleaned up dist.").then_some(())
    });
    assert!(s.open_interaction(&pane).is_none(), "AfterTool resolved it");
    let src = s.api("task.sources", json!({"run": run["id"]})).unwrap();
    assert_eq!(src["turns"][0]["prompt"], "clean up the build", "{src}");
}

#[test]
fn opencode_permission_ask_is_gated_with_a_user_asserted_capability() {
    let s = Session::new();
    // User manifest (04 §13 "user-asserted"): grant gate + native approval for any version.
    let hd = s.dir.path().join("harnesses");
    std::fs::create_dir_all(&hd).unwrap();
    std::fs::write(
        hd.join("opencode.toml"),
        "id = \"opencode\"\n[[capabilities]]\nversions = \"*\"\nmode = \"tui\"\nobserve = true\ngate = true\nanswer_native = [\"approval\"]\nanswer_keystroke = true\n",
    )
    .unwrap();
    let result = s.dir.path().join("decision.json");
    let fake = format!(
        r#"#!/bin/sh
# Fake OpenCode: what integrations/opencode-plugin/vibeke.ts sends.
h() {{ printf '%s' "$2" | "$VIBEKE_BIN" hook opencode "$1"; }}
h session.created '{{"info":{{"id":"ses_1","directory":"/tmp"}}}}' >/dev/null
h chat.message '{{"sessionID":"ses_1","text":"deploy it"}}' >/dev/null
h tool.execute.before '{{"tool":"bash","sessionID":"ses_1","callID":"c1","args":{{"command":"rm -rf dist"}}}}' >/dev/null
OUT=$(h permission.ask '{{"id":"per_1","type":"bash","pattern":"rm -rf dist","title":"Run rm -rf dist","sessionID":"ses_1","callID":"c1","metadata":{{}}}}')
printf '%s' "$OUT" > "{result}"
h tool.execute.after '{{"tool":"bash","sessionID":"ses_1","callID":"c1","metadata":{{"exit":0}}}}' >/dev/null
h message.updated '{{"info":{{"id":"m1","role":"assistant","tokens":{{"input":100,"output":20,"reasoning":5,"cache":{{"read":7,"write":1}}}},"cost":0.25,"modelID":"m","time":{{"completed":1}}}}}}' >/dev/null
h session.idle '{{"sessionID":"ses_1"}}' >/dev/null
sleep 60
"#,
        result = result.display()
    );
    let script = s.script("fake-opencode", &fake);
    let pane = s.pane(Path::new("/tmp"));
    s.json(&["pane", "run", &pane, &script.to_string_lossy()]);
    let it = s.until("gated permission", 15, || {
        s.open_interaction(&pane).filter(|i| i["gate"] == true)
    });
    assert_eq!(it["native_ref"], "per_1");
    assert_eq!(it["answer_channel"], "Native");
    assert_eq!(it["title"], "Run rm -rf dist");
    let run = s.run_of(&pane).unwrap();
    assert_eq!(run["harness"], "opencode");
    assert_eq!(run["integration"], "extension");
    s.api(
        "interaction.answer",
        json!({"interaction": it["id"], "decision": "allow"}),
    )
    .unwrap();
    let out = s.until("hook printed the decision", 15, || {
        std::fs::read_to_string(&result)
            .ok()
            .filter(|t| !t.is_empty())
    });
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap(),
        json!({"status": "allow"})
    );
    let r = s.until("idle with usage", 15, || {
        let r = s.run_of(&pane)?;
        (r["execution"]["value"] == "Idle" && r["usage"]["input_tokens"] == 100).then_some(r)
    });
    assert_eq!(r["usage"]["output_tokens"], 25);
    assert_eq!(r["usage"]["cache_read_tokens"], 7);
    assert_eq!(r["usage"]["cost_usd"], 0.25);
    let it = s
        .api("interaction.get", json!({"interaction": it["id"]}))
        .unwrap();
    assert_eq!(it["interaction"]["delivery"], "Delivered", "{it}");
}

const FAKE_ACP: &str = r#"
import json, sys
log = open(sys.argv[1], "a")
def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
prompt_id = None
for line in sys.stdin:
    m = json.loads(line)
    meth, mid = m.get("method"), m.get("id")
    if meth == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": False}}})
    elif meth == "session/new":
        log.write("cwd:" + m["params"]["cwd"] + "\n"); log.flush()
        send({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": "fake-sess-1"}})
    elif meth == "session/prompt":
        prompt_id = mid
        log.write("prompt:" + m["params"]["prompt"][0]["text"] + "\n"); log.flush()
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fake-sess-1", "update": {"sessionUpdate": "tool_call", "toolCallId": "tc1", "title": "cargo test", "kind": "execute", "status": "pending", "rawInput": {"command": "cargo test"}}}})
        send({"jsonrpc": "2.0", "id": 900, "method": "session/request_permission", "params": {"sessionId": "fake-sess-1", "toolCall": {"toolCallId": "tc1"}, "options": [
            {"optionId": "allow-1", "name": "Allow once", "kind": "allow_once"},
            {"optionId": "always-1", "name": "Allow always", "kind": "allow_always"},
            {"optionId": "reject-1", "name": "Reject", "kind": "reject_once"}]}})
    elif meth is None and mid == 900:
        outcome = m.get("result", {}).get("outcome", {})
        log.write("permission:" + outcome.get("optionId", "cancelled") + "\n"); log.flush()
        ok = outcome.get("optionId", "").startswith(("allow", "always"))
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fake-sess-1", "update": {"sessionUpdate": "tool_call_update", "toolCallId": "tc1", "status": "completed" if ok else "failed", "rawOutput": {"exit_code": 0}}}})
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fake-sess-1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Tests pass." if ok else "Skipped."}}}})
        send({"jsonrpc": "2.0", "id": prompt_id, "result": {"stopReason": "end_turn", "usage": {"inputTokens": 42, "outputTokens": 7}}})
    elif meth == "session/cancel":
        log.write("cancel\n"); log.flush()
"#;

fn python3() -> Option<String> {
    for p in [
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
        "/usr/bin/python3",
    ] {
        if Path::new(p).exists()
            && Command::new(p)
                .arg("-c")
                .arg("import json")
                .output()
                .is_ok_and(|o| o.status.success())
        {
            return Some(p.to_string());
        }
    }
    None
}

#[test]
fn acp_agent_runs_through_the_host_with_native_permission_answers() {
    let Some(py) = python3() else {
        eprintln!("skipping: no python3 for the fake ACP agent");
        return;
    };
    let s = Session::new();
    let agent = s.dir.path().join("fake_acp.py");
    std::fs::write(&agent, FAKE_ACP).unwrap();
    let log = s.dir.path().join("acp.log");
    let work = s.dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let pane = s.pane(&work);
    let started = s
        .api(
            "agent.start",
            json!({"pane": pane, "acp": format!("{py} {} {}", agent.display(), log.display()), "name": "acpbot"}),
        )
        .unwrap();
    assert_eq!(started["run"]["harness"], "acp:python3", "{started}");
    let run = s.until("ACP session identified", 20, || {
        s.run_of(&pane).filter(|r| {
            r["harness_session_id"] == "fake-sess-1" && r["execution"]["value"] == "Idle"
        })
    });
    assert_eq!(run["integration"], "acp");
    assert!(
        run["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("answer_native:approval"))
    );
    s.api(
        "agent.prompt",
        json!({"target": pane, "text": "run the tests"}),
    )
    .unwrap();
    let it = s.until("ACP permission interaction", 20, || {
        s.open_interaction(&pane)
    });
    assert_eq!(it["native_ref"], "tc1");
    assert_eq!(it["answer_channel"], "Native");
    assert_eq!(it["gate"], true, "no client focuses the pane: gate mode");
    assert_eq!(it["action"]["command"], "cargo test");
    s.api(
        "interaction.answer",
        json!({"interaction": it["id"], "decision": "allow_always"}),
    )
    .unwrap();
    let r = s.until("turn completed", 20, || {
        let r = s.run_of(&pane)?;
        (r["execution"]["value"] == "Idle" && r["last_message"] == "Tests pass.").then_some(r)
    });
    assert_eq!(r["usage"]["input_tokens"], 42);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("prompt:run the tests"), "{text}");
    assert!(
        text.contains("permission:always-1"),
        "the ACP agent received the mapped optionId: {text}"
    );
    assert!(
        text.contains(&format!("cwd:{}", work.canonicalize().unwrap().display()))
            || text.contains(&format!("cwd:{}", work.display())),
        "{text}"
    );
    let src = s.api("task.sources", json!({"run": r["id"]})).unwrap();
    assert_eq!(src["turns"][0]["prompt"], "run the tests", "{src}");
    let it = s
        .api("interaction.get", json!({"interaction": it["id"]}))
        .unwrap();
    assert_eq!(it["interaction"]["delivery"], "Delivered", "{it}");
    // The host's own transcript is in the pane.
    let screen = s.json(&["pane", "read", &pane]);
    let shown = screen.to_string();
    assert!(
        shown.contains("allow") || shown.contains("Allow always"),
        "{shown}"
    );
}

#[test]
fn herdr_self_report_with_seq_and_blocked() {
    let s = Session::new();
    let pane = s.pane(Path::new("/tmp"));
    let report = |state: &str, seq: i64| {
        s.api(
            "pane.report_agent",
            json!({"pane_id": pane, "source": "herdr:hermes", "agent": "hermes", "state": state, "seq": seq, "message": "waiting for approval"}),
        )
        .unwrap()
    };
    report("working", 10);
    let r = s.run_of(&pane).unwrap();
    assert_eq!(r["harness"], "hermes");
    assert_eq!(r["integration"], "self_report");
    assert_eq!(r["execution"]["value"], "Working");
    assert_eq!(r["execution"]["source"], "SelfReport");
    assert_eq!(report("idle", 9)["dropped"], "stale_seq");
    assert_eq!(s.run_of(&pane).unwrap()["execution"]["value"], "Working");
    report("blocked", 11);
    let it = s.open_interaction(&pane).expect("provisional approval");
    assert_eq!(it["source"], "SelfReport");
    assert!((it["confidence"].as_f64().unwrap() - 0.8).abs() < 1e-6);
    report("idle", 12);
    assert!(s.open_interaction(&pane).is_none());
    assert_eq!(s.run_of(&pane).unwrap()["execution"]["value"], "Idle");
    s.api(
        "pane.report_agent_session",
        json!({"pane_id": pane, "source": "herdr:hermes", "agent": "hermes", "agent_session_id": "h-42", "seq": 13}),
    )
    .unwrap();
    let r = s.run_of(&pane).unwrap();
    assert_eq!(r["harness_session_id"], "h-42");
    assert_eq!(r["resume_argv"], json!(["hermes", "--resume", "h-42"]));
    let e = s
        .api(
            "pane.report_agent",
            json!({"pane_id": pane, "state": "dancing", "seq": 99}),
        )
        .unwrap_err();
    assert!(e.to_string().contains("state"), "{e}");
}

#[test]
fn repo_manifests_need_trust_and_are_namespaced() {
    let s = Session::new();
    let repo = s.dir.path().join("repo");
    let hd = repo.join(".vibeke/harnesses");
    std::fs::create_dir_all(&hd).unwrap();
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .env("PATH", SAFE_PATH)
            .status()
            .is_ok_and(|st| st.success())
    );
    std::fs::write(
        hd.join("mytool.toml"),
        "id = \"mytool\"\nname = \"My tool\"\n[[detect.process]]\nexe_basename = [\"mytool-agent\"]\n[screen]\nmanifest = \"generic-repl\"\n",
    )
    .unwrap();
    s.script(
        "repo/mytool-agent",
        "#!/bin/sh\necho 'mytool ready'\nsleep 60\n",
    );
    let pane = s.pane(&repo);
    let listed = |id: &str| {
        s.api("agent.manifests", json!({})).unwrap()["manifests"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["id"] == id)
    };
    // Untrusted: the tool runs but nothing detects it.
    s.json(&["pane", "run", &pane, "./mytool-agent"]);
    s.until("tool in the foreground", 10, || {
        let p = s.json(&["pane", "get", &pane]);
        p["pane"]["fg_cmdline"]
            .to_string()
            .contains("mytool-agent")
            .then_some(())
    });
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        !listed("repo:mytool"),
        "untrusted repo manifests never load"
    );
    assert!(s.run_of(&pane).is_none());
    let _ = s.cmd(&["pane", "send-keys", &pane, "ctrl+c"]).output();
    s.api("policy.trust", json!({"path": repo})).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    s.json(&["pane", "run", &pane, "./mytool-agent"]);
    let r = s.until("detected via repo manifest after trust", 20, || {
        s.run_of(&pane).filter(|r| r["harness"] == "repo:mytool")
    });
    assert_eq!(r["capabilities"], json!(["observe"]));
    assert!(listed("repo:mytool"));
    assert!(!listed("mytool"), "never un-namespaced");
}

#[test]
fn integration_doctor_reports_version_against_validated_range() {
    let mut s = Session::new();
    let bin = s.dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    s.script("bin/opencode", "#!/bin/sh\necho 'opencode 0.99.0'\n");
    s.script("bin/claude", "#!/bin/sh\necho '2.1.290 (Claude Code)'\n");
    s.path = format!("{}:{SAFE_PATH}", bin.display());
    let out = s.cmd(&["integration", "doctor"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let row = |id: &str| {
        v["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id} missing: {v}"))
    };
    let oc = row("opencode");
    assert_eq!(oc["version"], "0.99.0");
    assert_eq!(oc["status"], "unvalidated");
    assert_eq!(oc["capabilities"], json!(["observe", "answer_keystroke"]));
    assert!(oc["validated_range"].is_null());
    let cl = row("claude");
    assert_eq!(cl["status"], "validated");
    assert_eq!(cl["validated_range"], ">=2.1.0, <2.2.0");
    assert_eq!(row("gemini")["status"], "not_installed");
    // Integration status reads only the redirected HOME (never the user's real configs).
    assert!(
        oc["integration"]["file"]
            .as_str()
            .unwrap()
            .starts_with(&s.dir.path().to_string_lossy().to_string())
    );
}
