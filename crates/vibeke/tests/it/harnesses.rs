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
            "VIBEKE_PANE_ULID",
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
    let tr = s.api("policy.trust", json!({"path": repo})).unwrap();
    assert_eq!(
        tr["harness_manifests"][0]["id"], "repo:mytool",
        "repo manifests are shown at trust time: {tr}"
    );
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
    // Keystrokes cannot answer OpenCode's dialog: an unvalidated version only observes.
    assert_eq!(oc["capabilities"], json!(["observe"]));
    assert_eq!(oc["validated_range"], ">=1.19.0, <1.20.0");
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

/// sha256 of `bytes` as lowercase hex, through the system tool (the test crate has no hasher).
fn sha256_hex(bytes: &[u8]) -> String {
    use std::io::Write;
    for (tool, args) in [("shasum", &["-a", "256"][..]), ("sha256sum", &[][..])] {
        let Ok(mut child) = Command::new(tool)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
        else {
            continue;
        };
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        let out = child.wait_with_output().unwrap();
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            return s.split_whitespace().next().unwrap().to_string();
        }
    }
    panic!("no shasum or sha256sum");
}

/// The signed manifest channel over the API (04 §13): `agent.manifests_check` refuses an
/// unsigned index and caches nothing; with the development opt-in it applies a built-in
/// harness's manifest from a local `file://` index, announces it once and refuses a replay;
/// `agent.manifest_pin` freezes only a cached version and unpins; `agent.drift` reports
/// per-version counters. Nothing leaves the machine: the index is in the test's temp dir.
#[test]
fn manifest_channel_check_pin_and_drift_over_the_api() {
    let s = Session::new();
    // No background poll: only the explicit checks below touch the channel.
    std::fs::write(
        s.dir.path().join("config.toml"),
        "[update]\nmanifest_check = false\nversion_check = false\n",
    )
    .unwrap();
    let pub_dir = s.dir.path().join("channel");
    std::fs::create_dir_all(&pub_dir).unwrap();
    let body =
        "id = \"gemini\"\n[[screen.rules]]\nid = \"x\"\nstate = \"working\"\nany = ['busy']\n";
    std::fs::write(pub_dir.join("gemini.toml"), body).unwrap();
    let index = json!({"serial": 3, "created_at": "2026-10-07", "manifests": [
        {"id": "gemini", "version": "2", "sha256": sha256_hex(body.as_bytes()), "url": "gemini.toml"},
        {"id": "not-a-harness", "version": "1", "sha256": "x", "url": "nope.toml"}
    ]});
    std::fs::write(pub_dir.join("index.json"), index.to_string()).unwrap();
    let url = format!("file://{}/index.json", pub_dir.display());

    // Unsigned and no opt-in: refused, nothing cached, nothing to pin.
    let e = s
        .api("agent.manifests_check", json!({"url": url}))
        .unwrap_err();
    assert!(e.to_string().contains("sign"), "{e}");
    let e = s
        .api("agent.manifest_pin", json!({"id": "gemini"}))
        .unwrap_err();
    assert!(
        e.to_string().contains("no remote manifests installed"),
        "{e}"
    );
    assert!(!s.dir.path().join("state/manifests/remote").exists());

    // The development opt-in is read by the server process: restart it with the variable.
    s.json(&["server", "stop", "--kill-panes"]);
    let pid: i32 = std::fs::read_to_string(s.dir.path().join("run/default/server.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: probing a pid with signal 0 sends nothing.
    s.until("the old server gone", 10, || {
        (unsafe { libc::kill(pid, 0) } != 0).then_some(())
    });
    let check = || {
        let out = s
            .cmd(&[
                "api",
                "call",
                "agent.manifests_check",
                &json!({"url": url}).to_string(),
            ])
            .env("VIBEKE_ALLOW_UNSIGNED_MANIFESTS", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let r = check();
    assert_eq!(r["serial"], 3, "{r}");
    assert_eq!(r["applied"], json!(["gemini@2"]), "{r}");
    assert_eq!(r["unsigned"], true);
    assert!(
        r["warnings"][0].as_str().unwrap().contains("UNSIGNED"),
        "{r}"
    );
    assert_eq!(r["announced"], 1);
    assert!(
        s.dir
            .path()
            .join("state/manifests/remote/gemini.toml")
            .is_file()
    );
    let loaded = s.json(&[
        "api",
        "call",
        "events.read",
        &json!({"types": ["harness.manifest_loaded"]}).to_string(),
    ]);
    let ev = &loaded["events"][0];
    assert_eq!(ev["data"]["id"], "gemini", "{loaded}");
    assert_eq!(ev["data"]["source"], "remote");
    assert_eq!(ev["data"]["verified"], "unsigned-dev");
    // The same serial again is not newer: nothing changes.
    let again = check();
    assert_eq!(again["unchanged"], true, "{again}");
    assert_eq!(again["index_serial"], 3);

    // Pin: only the cached version; unpin reports whether there was a pin.
    let e = s
        .api(
            "agent.manifest_pin",
            json!({"id": "gemini", "version": "9"}),
        )
        .unwrap_err();
    assert!(e.to_string().contains("not cached"), "{e}");
    let e = s
        .api("agent.manifest_pin", json!({"id": "codex"}))
        .unwrap_err();
    assert!(e.to_string().contains("no cached remote manifest"), "{e}");
    let pin = s
        .api("agent.manifest_pin", json!({"id": "gemini"}))
        .unwrap();
    assert_eq!(pin["version"], "2", "{pin}");
    assert!(pin["pinned_at_ms"].as_i64().unwrap() > 0);
    let pins: Value = serde_json::from_str(
        &std::fs::read_to_string(s.dir.path().join("state/manifests/pins.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(pins["gemini"]["version"], "2", "{pins}");
    let un = s
        .api("agent.manifest_pin", json!({"id": "gemini", "unpin": true}))
        .unwrap();
    assert_eq!(un["unpinned"], true, "{un}");
    let un = s
        .api("agent.manifest_pin", json!({"id": "gemini", "unpin": true}))
        .unwrap();
    assert_eq!(un["unpinned"], false, "{un}");

    // Drift counters: no harness version has been observed drifting in this session.
    let drift = s.json(&["agent", "drift"]);
    let versions = drift["versions"].as_array().expect("versions");
    assert!(versions.iter().all(|v| v["drifting"] == false), "{drift}");
}

/// `sandbox.setup_token` (13 §6): the token comes from stdin (prose around it, as
/// `claude setup-token` prints it) and lands in the session's private credentials dir, 0600 in a
/// 0700 dir; something that is not a token is refused and the stored one stays.
#[test]
fn sandbox_setup_token_stores_the_token_from_stdin_privately() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let s = Session::new();
    let token = "sk-ant-oat01-FAKEFAKEFAKEFAKEFAKE";
    let mut child = s
        .cmd(&["sandbox", "setup-token"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            format!("Your OAuth token (valid for 1 year):\n\n{token}\n\nStore it securely.\n")
                .as_bytes(),
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["stored"], true, "{r}");
    assert_eq!(r["env"], "CLAUDE_CODE_OAUTH_TOKEN");
    let path = PathBuf::from(r["path"].as_str().unwrap());
    assert_eq!(
        path,
        s.dir.path().join("state/credentials/claude-oauth-token")
    );
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains(token),
        "the token is never echoed"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{token}\n")
    );
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);

    let e = s
        .api(
            "sandbox.setup_token",
            json!({"token": "not a token at all"}),
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("does not look like a setup token"),
        "{e}"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("{token}\n")
    );
}

/// `sandbox.relaunch` (13 §3): a host run is restarted from its native session inside a
/// sandbox. It needs a live run with a resume handle and a containing level; the relaunched run
/// keeps the session, the old one ends, and the move is in the event log. The harness is a
/// fake `hermes` on the pane's PATH that prints the argv it was resumed with.
#[cfg(target_os = "macos")]
#[test]
fn sandbox_relaunch_moves_a_host_run_into_a_sandbox_from_its_session() {
    let mut s = Session::new();
    let work = s.dir.path().join("work");
    // Inside the workspace: the sandbox lets the agent read its checkout, not the rest of /tmp.
    let bin = work.join(".fakebin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        bin.join("hermes"),
        "#!/bin/sh\necho \"HERMES_ARGV $*\"\nexec sleep 60\n",
    )
    .unwrap();
    std::fs::set_permissions(
        bin.join("hermes"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    s.path = format!("{}:{SAFE_PATH}", bin.display());
    let pane = s.pane(&work);

    let e = s
        .api("sandbox.relaunch", json!({"run": "nope"}))
        .unwrap_err();
    assert!(e.to_string().contains("not found"), "{e}");
    // A host run that reported itself but has no native session yet: nothing to resume.
    s.api(
        "pane.report_agent",
        json!({"pane_id": pane, "source": "herdr:hermes", "agent": "hermes", "state": "working", "seq": 1}),
    )
    .unwrap();
    let old = s.run_of(&pane).unwrap();
    let old_id = old["id"].as_str().unwrap().to_string();
    let e = s
        .api("sandbox.relaunch", json!({"run": old_id}))
        .unwrap_err();
    assert!(e.to_string().contains("no resume handle"), "{e}");
    s.api(
        "pane.report_agent_session",
        json!({"pane_id": pane, "source": "herdr:hermes", "agent": "hermes", "agent_session_id": "h-7", "seq": 2}),
    )
    .unwrap();
    for (p, want) in [
        (
            json!({"run": old_id, "isolate": "host"}),
            "needs an isolation level",
        ),
        (
            json!({"run": old_id, "isolate": "moon"}),
            "unknown isolation level",
        ),
        (
            json!({"run": old_id, "isolate": "sandbox", "network": "moon"}),
            "unknown network profile",
        ),
    ] {
        let e = s.api("sandbox.relaunch", p.clone()).unwrap_err();
        assert!(e.to_string().contains(want), "{p}: {e}");
    }
    assert_eq!(
        s.run_of(&pane).unwrap()["id"],
        old_id.as_str(),
        "refusals change nothing"
    );

    let r = s
        .api(
            "sandbox.relaunch",
            json!({"run": old_id, "isolate": "sandbox", "network": "none"}),
        )
        .unwrap();
    assert_eq!(r["from"], old_id.as_str(), "{r}");
    assert_eq!(r["level"], "sandbox");
    let new_id = r["run"]["id"].as_str().unwrap().to_string();
    assert_ne!(new_id, old_id);
    assert_eq!(r["run"]["harness"], "hermes");
    assert_eq!(r["run"]["harness_session_id"], "h-7");
    assert_eq!(
        r["run"]["resume_argv"],
        json!(["hermes", "--resume", "h-7"])
    );
    let listed = s.run_of(&pane);
    assert_eq!(
        listed.as_ref().map(|r| r["id"].clone()),
        Some(json!(new_id)),
        "{}\n{}",
        s.json(&["pane", "read", &pane]),
        s.json(&["agent", "list"])
    );
    let ev = s.json(&[
        "api",
        "call",
        "events.read",
        &json!({"types": ["sandbox.relaunched", "agent.started"]}).to_string(),
    ]);
    let evs = ev["events"].as_array().unwrap();
    assert!(
        evs.iter().any(|e| e["type"] == "sandbox.relaunched"
            && e["data"]["from"] == old_id.as_str()
            && e["data"]["level"] == "sandbox"),
        "{ev}"
    );
    assert!(
        evs.iter().any(|e| e["type"] == "agent.started"
            && e["data"]["via"] == "relaunch"
            && e["data"]["resumed_from"] == old_id.as_str()),
        "{ev}"
    );
    // The resumed harness really runs in the pane, from its session.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let screen = s.json(&["pane", "read", &pane]).to_string();
        if screen.contains("HERMES_ARGV --resume h-7") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the resumed harness in the pane: {screen}"
        );
        std::thread::sleep(Duration::from_millis(150));
    }
    // The old run is over: relaunching it again is refused.
    let e = s
        .api(
            "sandbox.relaunch",
            json!({"run": old_id, "isolate": "sandbox"}),
        )
        .unwrap_err();
    assert!(
        e.to_string().contains("ended") || e.to_string().contains("not found"),
        "{e}"
    );
}

/// Fake Claude Code (Python, so the hook's parent is not a shell) reporting through the real
/// hook shim. `claude <dir>` starts with SessionStart, `late <dir>` without it (a run bound mid
/// session). Mid turn it runs a nested fake Codex as a child process, like a Bash tool call.
const FAKE_CLAUDE: &str = r#"
import json, os, subprocess, sys, time
VB = os.environ["VIBEKE_BIN"]
def h(harness, ev, p):
    subprocess.run([VB, "hook", harness, ev], input=json.dumps(p).encode(),
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
mode, d = sys.argv[1], sys.argv[2]
if mode == "codex":
    c = {"session_id": "cx-1", "transcript_path": d + "/codex.jsonl"}
    h("codex", "SessionStart", dict(c, source="startup"))
    h("codex", "UserPromptSubmit", dict(c, prompt="review the diff"))
    h("codex", "Stop", dict(c, last_assistant_message="no findings"))
    sys.exit(0)
base = {"session_id": "cl-1", "transcript_path": d + "/claude.jsonl"}
if mode == "claude":
    h("claude", "SessionStart", dict(base, source="startup"))
h("claude", "UserPromptSubmit", dict(base, prompt="review and merge"))
open(d + "/ready", "w").close()
while not os.path.exists(d + "/go"):
    time.sleep(0.05)
tool = dict(base, tool_name="Bash", tool_input={"command": "codex exec"}, tool_use_id="t1")
h("claude", "PreToolUse", tool)
subprocess.run([sys.executable, sys.argv[0], "codex", d])
h("claude", "PostToolUse", dict(tool, tool_response={"stdout": "no findings"}))
h("claude", "Stop", dict(base, last_assistant_message="Merged."))
open(d + "/done", "w").close()
time.sleep(60)
"#;

fn run_fake_claude(mode: &str) -> Option<(Session, String, PathBuf)> {
    let py = python3()?;
    let s = Session::new();
    let agent = s.dir.path().join("fake_claude.py");
    std::fs::write(&agent, FAKE_CLAUDE).unwrap();
    let d = s.dir.path().join("io");
    std::fs::create_dir_all(&d).unwrap();
    let pane = s.pane(Path::new("/tmp"));
    let line = format!("{py} {} {mode} {}", agent.display(), d.display());
    s.json(&["pane", "run", &pane, &line]);
    Some((s, pane, d))
}

#[test]
fn nested_agent_does_not_replace_the_pane_agent() {
    let Some((s, pane, d)) = run_fake_claude("claude") else {
        eprintln!("skipping: no python3 for the fake harness");
        return;
    };
    s.until("claude ready", 20, || {
        d.join("ready").exists().then_some(())
    });
    let run = s.until("claude run identified", 15, || {
        s.run_of(&pane)
            .filter(|r| r["harness"] == "claude" && r["harness_session_id"] == "cl-1")
    });
    std::fs::write(d.join("go"), "").unwrap();
    s.until("turn done", 20, || d.join("done").exists().then_some(()));
    let after = s.until("turn finished", 15, || {
        s.run_of(&pane).filter(|r| r["last_message"] == "Merged.")
    });
    assert_eq!(
        after["id"], run["id"],
        "the nested codex replaced the run: {after}"
    );
    assert_eq!(after["harness"], "claude");
    assert_eq!(
        after["transcript_path"],
        format!("{}/claude.jsonl", d.display())
    );
}

#[test]
fn run_bound_mid_session_learns_its_transcript_from_any_hook() {
    let Some((s, pane, d)) = run_fake_claude("late") else {
        eprintln!("skipping: no python3 for the fake harness");
        return;
    };
    s.until("claude ready", 20, || {
        d.join("ready").exists().then_some(())
    });
    let run = s.until("claude run identified", 15, || {
        s.run_of(&pane)
            .filter(|r| r["harness"] == "claude" && r["harness_session_id"] == "cl-1")
    });
    assert_eq!(
        run["transcript_path"],
        format!("{}/claude.jsonl", d.display())
    );
    assert_eq!(run["resume_argv"], json!(["claude", "--resume", "cl-1"]));
}

/// Fake Codex whose process also runs a sub-agent thread and a memory consolidation thread
/// (no rollout): their hooks come from the same process as the pane's thread.
const FAKE_CODEX_THREADS: &str = r#"
import json, os, subprocess, sys, time
VB = os.environ["VIBEKE_BIN"]
def h(ev, p):
    subprocess.run([VB, "hook", "codex", ev], input=json.dumps(p).encode(),
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
d = sys.argv[1]
root = {"session_id": "cx-root", "transcript_path": d + "/root.jsonl", "cwd": d}
child = {"session_id": "cx-child", "transcript_path": d + "/child.jsonl", "cwd": d}
mem = {"session_id": "cx-mem", "transcript_path": None, "cwd": d}
h("SessionStart", dict(root, source="startup"))
h("UserPromptSubmit", dict(root, prompt="split the work"))
h("SubagentStart", dict(root, agent_id="cx-child", agent_type="default"))
h("SessionStart", dict(mem, source="startup"))
h("UserPromptSubmit", dict(mem, prompt="Consolidate memories"))
h("SessionEnd", dict(mem, reason="other"))
h("UserPromptSubmit", dict(child, agent_id="cx-child", prompt="do part one"))
h("PostToolUse", dict(child, agent_id="cx-child", tool_name="Bash",
                      tool_input={"command": "ls"}, tool_use_id="ct1", tool_response="ok"))
h("Stop", dict(child, last_assistant_message="part one done"))
open(d + "/threads", "w").close()
while not os.path.exists(d + "/go"):
    time.sleep(0.05)
h("SubagentStop", dict(root, agent_id="cx-child", agent_type="default"))
h("Stop", dict(root, last_assistant_message="All parts done."))
open(d + "/done", "w").close()
time.sleep(60)
"#;

#[test]
fn codex_subagent_and_internal_thread_hooks_leave_the_pane_run_alone() {
    let Some(py) = python3() else {
        eprintln!("skipping: no python3 for the fake harness");
        return;
    };
    let s = Session::new();
    let agent = s.dir.path().join("fake_codex.py");
    std::fs::write(&agent, FAKE_CODEX_THREADS).unwrap();
    let d = s.dir.path().join("io");
    std::fs::create_dir_all(&d).unwrap();
    let pane = s.pane(Path::new("/tmp"));
    let line = format!("{py} {} {}", agent.display(), d.display());
    s.json(&["pane", "run", &pane, &line]);
    s.until("other threads reported", 20, || {
        d.join("threads").exists().then_some(())
    });
    let run = s.until("codex run identified", 15, || {
        s.run_of(&pane)
            .filter(|r| r["harness"] == "codex" && r["harness_session_id"] == "cx-root")
    });
    assert_eq!(
        run["transcript_path"],
        format!("{}/root.jsonl", d.display()),
        "{run}"
    );
    assert_eq!(
        run["execution"]["value"], "Working",
        "a child's Stop does not end the turn: {run}"
    );
    assert_ne!(run["last_message"], "part one done", "{run}");
    std::fs::write(d.join("go"), "").unwrap();
    s.until("turn done", 20, || d.join("done").exists().then_some(()));
    let after = s.until("turn finished", 15, || {
        s.run_of(&pane)
            .filter(|r| r["last_message"] == "All parts done.")
    });
    assert_eq!(after["id"], run["id"], "{after}");
    assert_eq!(after["harness_session_id"], "cx-root");
    assert_eq!(after["execution"]["value"], "Idle");
}

/// Fake Claude Code for Send now: it records a queued message in its transcript, reads the pane
/// in raw mode and takes the queue when `chat:sendNow` (`ctrl+x ctrl+s`) arrives.
const FAKE_CLAUDE_QUEUE: &str = r#"
import datetime, json, os, subprocess, sys, time, tty
VB = os.environ["VIBEKE_BIN"]
d = sys.argv[1]
tp = d + "/claude.jsonl"
def h(ev, p):
    subprocess.run([VB, "hook", "claude", ev], input=json.dumps(p).encode(),
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
def now():
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%f")[:-3] + "Z"
def line(o):
    with open(tp, "a") as f:
        f.write(json.dumps(o) + "\n")
line({"type": "user", "timestamp": now(), "uuid": "u1", "message": {"role": "user", "content": "long job"}})
base = {"session_id": "cl-q", "transcript_path": tp}
h("SessionStart", dict(base, source="startup"))
h("UserPromptSubmit", dict(base, prompt="long job"))
line({"type": "queue-operation", "operation": "enqueue", "timestamp": now(), "sessionId": "cl-q", "content": "also run the tests"})
tty.setraw(0)
open(d + "/ready", "w").close()
got = b""
while b"\x18\x13" not in got:
    got += os.read(0, 64)
with open(d + "/keys", "wb") as f:
    f.write(got)
line({"type": "queue-operation", "operation": "dequeue", "timestamp": now(), "sessionId": "cl-q"})
open(d + "/done", "w").close()
time.sleep(60)
"#;

#[test]
fn send_now_delivers_a_message_claude_queued() {
    let Some(py) = python3() else {
        eprintln!("skipping: no python3 for the fake harness");
        return;
    };
    let s = Session::new();
    let agent = s.dir.path().join("fake_claude_queue.py");
    std::fs::write(&agent, FAKE_CLAUDE_QUEUE).unwrap();
    let d = s.dir.path().join("io");
    std::fs::create_dir_all(&d).unwrap();
    let pane = s.pane(Path::new("/tmp"));
    let line = format!("{py} {} {}", agent.display(), d.display());
    s.json(&["pane", "run", &pane, &line]);
    s.until("claude ready", 20, || {
        d.join("ready").exists().then_some(())
    });
    let run = s.until("claude run identified", 15, || {
        s.run_of(&pane)
            .filter(|r| r["harness"] == "claude" && r["harness_session_id"] == "cl-q")
    });
    let id = run["id"].as_str().unwrap().to_string();
    let queued = |s: &Session| -> Option<Vec<Value>> {
        let v = s.api("agent.queued", json!({"target": id})).ok()?;
        v["items"].as_array().cloned()
    };
    let items = s.until("the queued message is listed", 20, || {
        queued(&s).filter(|it| !it.is_empty())
    });
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["text"], "also run the tests");
    assert_eq!(items[0]["source"], "transcript");
    assert!(items[0]["id"].as_str().is_some_and(|i| !i.is_empty()));

    // No keybindings.json in the test home: the default chord.
    let r = s
        .api("agent.send_now", json!({"target": id}))
        .expect("send now");
    assert_eq!(r["keys"], json!(["ctrl+x", "ctrl+s"]), "{r}");
    assert_eq!(r["sent"], 1);
    s.until("the harness took the queue", 15, || {
        d.join("done").exists().then_some(())
    });
    let keys = std::fs::read(d.join("keys")).unwrap();
    assert!(
        keys.windows(2).any(|w| w == b"\x18\x13"),
        "keys the harness read: {keys:?}"
    );
    s.until("the queue is empty", 20, || {
        queued(&s).filter(|it| it.is_empty())
    });
    // Nothing waits any more: Send now refuses instead of pressing keys.
    let e = s
        .api("agent.send_now", json!({"target": id}))
        .expect_err("nothing queued");
    assert!(e.to_string().contains("nothing"), "{e}");
}

// Codex's queue and inline question editor. These screens follow Codex's async_questions
// renderer: no progress line for one question, numbered choices plus Other, and a composer
// for free text. The fake records every input byte, so an unsolicited key is visible.
const FAKE_CODEX_QUESTIONS: &str = r#"
import datetime, json, os, select, subprocess, sys, time, tty
d = sys.argv[1]
tp = d + '/codex.jsonl'
def line(payload, kind='event_msg'):
    with open(tp, 'a') as f:
        f.write(json.dumps({'timestamp': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'type': kind, 'payload': payload}) + '\n')
def hook(ev, **p):
    p.update(session_id='cx-questions', transcript_path=tp)
    subprocess.run([os.environ['VIBEKE_BIN'], 'hook', 'codex', ev], input=json.dumps(p).encode(), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
line({'id':'cx-questions'}, 'session_meta')
hook('SessionStart', source='startup')
hook('UserPromptSubmit', prompt='prepare a preview')
if os.path.exists(d+'/large'):
    for _ in range(300): line({'type':'noop','padding':'x'*4096})
pending = [{'title':'Which environment?', 'options':['Test','Production'], 'id':0}, {'title':'Any constraints?', 'id':1}]
line({'type':'item_completed', 'item':{'type':'AgentMessage', 'id':'call_q', 'delivery':'async', 'questions':pending}})
line({'type':'function_call_output','call_id':'call_q','output':'{"accepted":true}'}, 'response_item')
tty.setraw(0)
expanded, index, pointer, draft = False, 0, 0, ''
delayed=[]
def draw():
    out = '\x1b[?2004h\x1b[H\x1b[2J• Working (1s • esc to interrupt)\r\n\r\n'
    if pending:
        out += '• Queued follow-up inputs\r\n'
        if not expanded:
            out += '  ? %d question%s\r\n    ⇧← to answer\r\n\r\n› Ask Codex to do anything\r\n' % (len(pending), '' if len(pending)==1 else 's')
        else:
            if len(pending)>1: out += '\r\n  %d of %d\r\n' % (index+1,len(pending))
            q = pending[index]
            out += '\r\n  '+q['title']+'\r\n\r\n'
            if q.get('options'):
                for n,label in enumerate(q['options']+[(draft or 'Other (write an answer)') if pointer==2 else 'Other']):
                    out += '  %s %d. %s\r\n' % ('›' if n==pointer else ' ',n+1,label)
            else: out += '  '+(draft or 'Type your answer')+'\r\n'
            out += '\r\n  enter submit   ⌃] skip   ⇧→ '+('prev question' if index else 'main prompt')
            if index+1<len(pending): out += '   ⇧← next question'
            out += '\r\n'
    else: out += '› Ask Codex to do anything\r\n'
    os.write(1,out.encode())
draw()
open(d+'/ready','w').close()
buf=b''
while True:
    if os.path.exists(d+'/replay-marker'):
        os.unlink(d+'/replay-marker')
        line({'type':'function_call','call_id':'replay-ready','name':'exec_command','arguments':json.dumps({'cmd':'printf replay-ready'})},'response_item')
        line({'type':'function_call_output','call_id':'replay-ready','output':'Process running with session ID 424242'},'response_item')
    if os.path.exists(d+'/flush-late'):
        os.unlink(d+'/flush-late')
        line({'type':'task_complete','turn_id':'t1'})
        for reply in delayed: line({'type':'user_message','message':reply})
        delayed=[]
    if os.path.exists(d+'/skip-one'):
        os.unlink(d+'/skip-one')
        pending.pop(0)
        expanded,index,pointer,draft=False,0,0,''
        draw()
    if os.path.exists(d+'/replace'):
        os.unlink(d+'/replace')
        pending[0]['title']='A replacement question?'
        open(d+'/replaced','w').close()
        draw()
    if os.path.exists(d+'/work'):
        os.unlink(d+'/work')
        hook('PreToolUse', tool_name='Bash', tool_use_id='work', tool_input={'command':'check preview'})
        hook('PostToolUse', tool_name='Bash', tool_use_id='work', tool_input={'command':'check preview'})
        open(d+'/worked','w').close()
        draw()
    if os.path.exists(d+'/skip'):
        os.unlink(d+'/skip')
        pending=[]
        draw()
    if not select.select([0],[],[],0.1)[0]: continue
    data=os.read(0,4096)
    if not data: break
    with open(d+'/keys','ab') as f: f.write(data)
    buf+=data
    while buf:
        if buf.startswith(b'\x1b[200~'):
            if b'\x1b[201~' not in buf: break
            end=buf.index(b'\x1b[201~')
            draft+=buf[6:end].decode()
            buf=buf[end+6:]
        elif buf.startswith(b'\x1b'):
            seq=next((s for s in [b'\x1b[1;2D',b'\x1b[1;2C',b'\x1b[B',b'\x1bOB',b'\x1b[A',b'\x1bOA'] if buf.startswith(s)),None)
            if seq is None: break
            buf=buf[len(seq):]
            if seq==b'\x1b[1;2D':
                if not expanded: expanded=True
                elif index+1<len(pending): index+=1
                pointer,draft=0,''
            elif seq==b'\x1b[1;2C':
                if index: index-=1; pointer,draft=0,''
                else: expanded=False
            elif seq in [b'\x1b[B',b'\x1bOB']: pointer=min(pointer+1,2)
            elif seq in [b'\x1b[A',b'\x1bOA']: pointer=max(pointer-1,0)
        elif buf[0]==13:
            buf=buf[1:]
            if not expanded or not pending: continue
            if os.path.exists(d+'/reject'):
                os.write(1,b'\x1b[H\x1b[2JExpand terminal to read the entire option\r\n')
                continue
            q=pending[index]
            answer=draft or q.get('options',[''])[pointer]
            qid=json.dumps(['request_user_input_async','call_q',q['id']],separators=(',',':'))
            reply='<send_user_message_question_reply>'+json.dumps([{'questionItemId':qid,'question':q['title'],'answer':answer}])+'</send_user_message_question_reply>'
            if os.path.exists(d+'/late'): delayed.append(reply)
            else: line({'type':'user_message','message':reply})
            with open(d+'/answers','a') as f: f.write(json.dumps({'question':q['title'],'answer':answer})+'\n')
            pending.pop(index)
            expanded,index,pointer,draft=bool(pending),0,0,''
        else: buf=buf[1:]
        draw()
"#;

#[test]
fn codex_follow_up_questions_are_answered_while_the_turn_keeps_working() {
    let Some(py) = python3() else {
        return;
    };
    let s = Session::new();
    let agent = s.dir.path().join("fake_codex_questions.py");
    std::fs::write(&agent, FAKE_CODEX_QUESTIONS).unwrap();
    let d = s.dir.path().join("io");
    std::fs::create_dir_all(&d).unwrap();
    let pane = s.pane(Path::new("/tmp"));
    s.json(&[
        "pane",
        "run",
        &pane,
        &format!("{py} {} {}", agent.display(), d.display()),
    ]);
    let cards = || {
        let v = s.api("interaction.list", json!({})).ok()?;
        Some(
            v["interactions"]
                .as_array()?
                .iter()
                .filter(|i| i["pane"] == pane && i["status"] == "Open")
                .cloned()
                .collect::<Vec<_>>(),
        )
    };
    let questions = s.until("both follow-up question cards", 20, || {
        cards().filter(|q| q.len() == 2)
    });
    assert!(
        !d.join("keys").exists(),
        "observing a queued question must never type into Codex"
    );
    std::fs::write(d.join("work"), "").unwrap();
    s.until("Codex ran another tool", 15, || {
        d.join("worked").exists().then_some(())
    });
    assert_eq!(
        cards().unwrap().len(),
        2,
        "other work does not close asynchronous questions"
    );
    let free = questions
        .iter()
        .find(|q| q["title"] == "Any constraints?")
        .unwrap();
    s.api(
        "interaction.answer",
        json!({"interaction":free["id"],"text":"Keep the service running"}),
    )
    .unwrap();
    let delivered = |it: &Value| {
        s.until("verified answer delivery", 15, || {
            let v = s
                .api("interaction.get", json!({"interaction":it["id"]}))
                .ok()?;
            let delivery = &v["interaction"]["delivery"];
            assert!(
                !matches!(delivery.as_str(), Some("Failed" | "DeliveryUnknown")),
                "{v}"
            );
            (delivery == "Delivered").then_some(())
        });
    };
    delivered(free);
    let screen = s
        .api("pane.read", json!({"pane":pane, "source":"visible"}))
        .unwrap();
    assert!(
        screen["text"]
            .as_str()
            .unwrap_or_default()
            .contains("⇧← to answer"),
        "Vibeke must return to the main prompt: {screen}"
    );
    let choice = questions
        .iter()
        .find(|q| q["title"] == "Which environment?")
        .unwrap();
    let qid = choice["questions"][0]["id"].as_str().unwrap();
    s.api(
        "interaction.answer",
        json!({"interaction":choice["id"],"choices":{qid:["1"]}}),
    )
    .unwrap();
    delivered(choice);
    let answers: Vec<Value> = std::fs::read_to_string(d.join("answers"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        answers,
        vec![
            json!({"question":"Any constraints?","answer":"Keep the service running"}),
            json!({"question":"Which environment?","answer":"Production"})
        ]
    );
    s.until("question cards clear", 15, || {
        cards().filter(|q| q.is_empty())
    });
    assert_eq!(s.run_of(&pane).unwrap()["execution"]["value"], "Working");
}

#[test]
fn codex_follow_up_questions_skipped_in_the_pane_clear_without_an_answer() {
    let Some(py) = python3() else {
        return;
    };
    let s = Session::new();
    let agent = s.dir.path().join("fake_codex_questions.py");
    std::fs::write(&agent, FAKE_CODEX_QUESTIONS).unwrap();
    let d = s.dir.path().join("io");
    std::fs::create_dir_all(&d).unwrap();
    let pane = s.pane(Path::new("/tmp"));
    s.json(&[
        "pane",
        "run",
        &pane,
        &format!("{py} {} {}", agent.display(), d.display()),
    ]);
    s.until("queued question cards", 20, || s.open_interaction(&pane));
    std::fs::write(d.join("skip"), "").unwrap();
    s.until("skipped questions clear", 15, || {
        s.open_interaction(&pane).is_none().then_some(())
    });
    assert!(
        !d.join("keys").exists(),
        "detection and cleanup must never send input"
    );
    assert!(
        !d.join("answers").exists(),
        "skipping a question is not a user answer"
    );
}

#[test]
fn codex_follow_up_answer_refuses_a_replacement_question() {
    let Some(py) = python3() else {
        return;
    };
    let s = Session::new();
    let agent = s.dir.path().join("fake_codex_questions.py");
    std::fs::write(&agent, FAKE_CODEX_QUESTIONS).unwrap();
    let d = s.dir.path().join("io");
    std::fs::create_dir_all(&d).unwrap();
    let pane = s.pane(Path::new("/tmp"));
    s.json(&[
        "pane",
        "run",
        &pane,
        &format!("{py} {} {}", agent.display(), d.display()),
    ]);
    let question = s.until("question card", 20, || {
        s.open_interaction(&pane)
            .filter(|q| q["title"] == "Which environment?")
    });
    std::fs::write(d.join("replace"), "").unwrap();
    s.until("replacement question shown", 10, || {
        d.join("replaced").exists().then_some(())
    });
    let qid = question["questions"][0]["id"].as_str().unwrap();
    s.api(
        "interaction.answer",
        json!({"interaction":question["id"],"choices":{qid:["1"]}}),
    )
    .unwrap();
    s.until("stale answer refused", 15, || {
        let it = s
            .api("interaction.get", json!({"interaction":question["id"]}))
            .ok()?;
        (it["interaction"]["status"] == "Open" && it["interaction"]["delivery_error"].is_string())
            .then_some(())
    });
    assert!(!d.join("answers").exists());
    assert!(
        !std::fs::read(d.join("keys"))
            .unwrap_or_default()
            .contains(&b'\r'),
        "a changed question must never receive Enter"
    );
}

#[test]
fn codex_follow_up_cards_survive_replaying_a_large_session_after_restart() {
    let Some(py) = python3() else {
        return;
    };
    let s = Session::new();
    let agent = s.dir.path().join("fake_codex_questions.py");
    std::fs::write(&agent, FAKE_CODEX_QUESTIONS).unwrap();
    let d = s.dir.path().join("io");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("large"), "").unwrap();
    let pane = s.pane(Path::new("/tmp"));
    s.json(&[
        "pane",
        "run",
        &pane,
        &format!("{py} {} {}", agent.display(), d.display()),
    ]);
    let before = s.until("question after a large transcript", 30, || {
        s.open_interaction(&pane)
    });
    s.json(&["server", "stop"]);
    std::fs::write(d.join("replay-marker"), "").unwrap();
    s.until("session replay reached its end", 30, || {
        s.run_of(&pane)
            .filter(|r| r["bg_tasks"].to_string().contains("424242"))
    });
    let after = s.until("same card after restart and replay", 30, || {
        s.open_interaction(&pane)
    });
    assert_eq!(after["id"], before["id"]);
    assert!(!d.join("keys").exists());
}

#[test]
fn codex_follow_up_does_not_claim_delivery_when_codex_rejects_enter() {
    let Some(py) = python3() else {
        return;
    };
    let s = Session::new();
    let agent = s.dir.path().join("fake_codex_questions.py");
    std::fs::write(&agent, FAKE_CODEX_QUESTIONS).unwrap();
    let d = s.dir.path().join("io");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("reject"), "").unwrap();
    let pane = s.pane(Path::new("/tmp"));
    s.json(&[
        "pane",
        "run",
        &pane,
        &format!("{py} {} {}", agent.display(), d.display()),
    ]);
    let question = s.until("question card", 20, || {
        s.open_interaction(&pane)
            .filter(|q| q["title"] == "Which environment?")
    });
    let qid = question["questions"][0]["id"].as_str().unwrap();
    s.api(
        "interaction.answer",
        json!({"interaction":question["id"],"choices":{qid:["1"]}}),
    )
    .unwrap();
    s.until("delivery remains unconfirmed", 15, || {
        let it = s
            .api("interaction.get", json!({"interaction":question["id"]}))
            .ok()?;
        assert_ne!(it["interaction"]["delivery"], "Delivered", "{it}");
        (it["interaction"]["delivery"] == "DeliveryUnknown").then_some(())
    });
    assert!(!d.join("answers").exists());
}

#[test]
fn codex_follow_up_records_a_delayed_reply_after_the_turn_ends() {
    let Some(py) = python3() else {
        return;
    };
    let s = Session::new();
    let agent = s.dir.path().join("fake_codex_questions.py");
    std::fs::write(&agent, FAKE_CODEX_QUESTIONS).unwrap();
    let d = s.dir.path().join("io");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("late"), "").unwrap();
    let pane = s.pane(Path::new("/tmp"));
    s.json(&[
        "pane",
        "run",
        &pane,
        &format!("{py} {} {}", agent.display(), d.display()),
    ]);
    let question = s.until("question card", 20, || {
        s.open_interaction(&pane)
            .filter(|q| q["title"] == "Which environment?")
    });
    let qid = question["questions"][0]["id"].as_str().unwrap();
    s.api(
        "interaction.answer",
        json!({"interaction":question["id"],"choices":{qid:["1"]}}),
    )
    .unwrap();
    s.until("Codex accepted the answer into its queue", 10, || {
        let v = s
            .api("interaction.get", json!({"interaction":question["id"]}))
            .ok()?;
        (v["interaction"]["delivery"] == "DecisionRecorded").then_some(())
    });
    let remaining = s.until(
        "remaining question is answerable before the first reply is recorded",
        10,
        || {
            s.open_interaction(&pane)
                .filter(|q| q["title"] == "Any constraints?" && q["answerable"] == true)
        },
    );
    s.api(
        "interaction.answer",
        json!({"interaction":remaining["id"],"text":"No other constraints"}),
    )
    .unwrap();
    s.until(
        "second answer is recorded without waiting for the model",
        10,
        || {
            let v = s
                .api("interaction.get", json!({"interaction":remaining["id"]}))
                .ok()?;
            (v["interaction"]["delivery"] == "DecisionRecorded").then_some(())
        },
    );
    std::fs::write(d.join("flush-late"), "").unwrap();
    s.until("late reply confirms delivery", 15, || {
        let v = s
            .api("interaction.get", json!({"interaction":question["id"]}))
            .ok()?;
        (v["interaction"]["delivery"] == "Delivered").then_some(())
    });
}

#[test]
fn codex_follow_up_partial_skip_offers_the_pane_instead_of_endless_retries() {
    let Some(py) = python3() else {
        return;
    };
    let s = Session::new();
    let agent = s.dir.path().join("fake_codex_questions.py");
    std::fs::write(&agent, FAKE_CODEX_QUESTIONS).unwrap();
    let d = s.dir.path().join("io");
    std::fs::create_dir_all(&d).unwrap();
    let pane = s.pane(Path::new("/tmp"));
    s.json(&[
        "pane",
        "run",
        &pane,
        &format!("{py} {} {}", agent.display(), d.display()),
    ]);
    s.until("question cards", 20, || s.open_interaction(&pane));
    std::fs::write(d.join("skip-one"), "").unwrap();
    s.until("remaining question offers answer in pane", 15, || {
        let v = s.api("interaction.list", json!({})).ok()?;
        v["interactions"]
            .as_array()?
            .iter()
            .find(|q| {
                q["pane"] == pane && q["title"] == "Any constraints?" && q["answerable"] == false
            })
            .cloned()
    });
    assert!(!d.join("keys").exists());
}
