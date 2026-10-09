//! Headless harness runs end to end (01 §1.2 pipe mode, 04 §3.3/§6.1.3/§6.2/§6.3/§6.6) with
//! **fake harness binaries only**: small Python programs named `claude`, `codex` and `pi` that
//! speak each protocol as spec 04 describes it, placed first on a minimal PATH, plus a fake ACP
//! agent passed by absolute path. No real harness can be found or run (PATH holds only the
//! fakes and system directories; `CLAUDE_CONFIG_DIR`/`CODEX_HOME` point into the temp dir).
//!
//! Chaos (10 §5.1 "headless pipe-mode run"): the server is killed -9 mid-turn and with an
//! approval pending; the harness process survives in its pipe-mode holder, the restarted
//! server replays the journal, reconciles, and the turn completes exactly once.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

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

struct Session {
    dir: tempfile::TempDir,
    fakebin: PathBuf,
    log: PathBuf,
    turn_secs: &'static str,
}

impl Session {
    fn new(py: &str, turn_secs: &'static str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkhl")
            .tempdir_in("/tmp")
            .unwrap();
        let fakebin = dir.path().join("fakebin");
        std::fs::create_dir_all(&fakebin).unwrap();
        for (name, body) in [
            ("claude", FAKE_CLAUDE),
            ("codex", FAKE_CODEX),
            ("pi", FAKE_PI),
            ("fake-acp", FAKE_ACP),
        ] {
            let p = fakebin.join(name);
            std::fs::write(&p, format!("#!{py}\n{body}")).unwrap();
            std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        for d in ["home", "claude-config", "codex-home", "work"] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        let log = dir.path().join("fake.log");
        Session {
            dir,
            fakebin,
            log,
            turn_secs,
        }
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env("PATH", format!("{}:{SYSTEM_PATH}", self.fakebin.display()))
            .env("HOME", d.join("home"))
            .env("CLAUDE_CONFIG_DIR", d.join("claude-config"))
            .env("CODEX_HOME", d.join("codex-home"))
            .env("VKFAKE_LOG", &self.log)
            .env("VKFAKE_TURN_SECS", self.turn_secs);
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
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; fake log:\n{}",
                self.log_text()
            );
            std::thread::sleep(Duration::from_millis(150));
        }
    }
    fn workspace_pane(&self) -> String {
        let work = self.dir.path().join("work");
        self.json(&["workspace", "create", "--cwd", &work.to_string_lossy()])["root_pane"]["id"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn run(&self, id: &str) -> Value {
        self.live_run(id).unwrap_or(Value::Null)
    }
    /// A run while it lives (ended runs leave the model).
    fn live_run(&self, id: &str) -> Option<Value> {
        let v = self.api("agent.list", json!({})).ok()?;
        v["runs"]
            .as_array()?
            .iter()
            .find(|r| r["id"] == id)
            .cloned()
    }
    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
    fn log_count(&self, needle: &str) -> usize {
        self.log_text()
            .lines()
            .filter(|l| l.contains(needle))
            .count()
    }
    fn open_interactions(&self, run: &str) -> Vec<Value> {
        let v = self
            .api("interaction.list", json!({"run": run, "status": "open"}))
            .unwrap();
        // `interaction.list` has no run filter: keep the run's own.
        v["interactions"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| i["run"] == run)
            .collect()
    }
    fn events(&self, ty: &str) -> Vec<Value> {
        let v = self
            .api("events.read", json!({"types": [ty], "limit": 500}))
            .unwrap();
        v["events"].as_array().cloned().unwrap_or_default()
    }
    fn server_pid(&self) -> i32 {
        std::fs::read_to_string(self.dir.path().join("run/default/server.pid"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }
    fn kill_server(&self) {
        let pid = self.server_pid();
        assert!(pid > 0);
        // SAFETY: killing our own server process.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(pid) {
            assert!(Instant::now() < deadline, "server {pid} did not die");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    /// The fake harness's pid, from its first log line.
    fn harness_pid(&self, name: &str) -> i32 {
        self.log_text()
            .lines()
            .find_map(|l| l.strip_prefix(&format!("start:{name}:")))
            .and_then(|r| r.split(':').next())
            .and_then(|p| p.parse().ok())
            .expect("fake harness started")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn parent_pid(pid: i32) -> i32 {
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// `codex app-server` per 04 §6.2: initialize/thread/turn, an approval request per turn, the
/// turn finishing `VKFAKE_TURN_SECS` after the decision (so a test can kill the server
/// mid-turn), `thread/read` reporting the real status.
const FAKE_CODEX: &str = r#"
import json, os, sys, time
log = open(os.environ["VKFAKE_LOG"], "a")
def L(s): log.write(s + "\n"); log.flush()
def out(o): sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
L("start:codex:%d:%s" % (os.getpid(), " ".join(sys.argv[1:])))
L("env:CODEX_HOME=" + os.environ.get("CODEX_HOME", ""))
threads = 0; thread = None; busy = False; turn = None
for line in sys.stdin:
    m = json.loads(line)
    meth, mid = m.get("method"), m.get("id")
    if meth == "initialize":
        out({"id": mid, "result": {"userAgent": "fake-codex"}})
    elif meth == "initialized":
        L("initialized")
    elif meth == "thread/start":
        threads += 1; thread = "th-fake-%d" % threads
        out({"id": mid, "result": {"thread": {"id": thread}}})
        out({"method": "thread/started", "params": {"thread": {"id": thread}}})
    elif meth == "thread/read":
        L("thread/read")
        out({"id": mid, "result": {"thread": {"id": m["params"]["threadId"], "status": {"type": "active" if busy else "idle"}}}})
    elif meth == "turn/start":
        thread = m["params"]["threadId"]
        busy = True; turn = "turn-%s" % mid
        L("prompt:" + m["params"]["input"][0]["text"])
        out({"id": mid, "result": {"turn": {"id": turn, "status": "inProgress"}}})
        out({"method": "turn/started", "params": {"threadId": thread, "turn": {"id": turn}}})
        out({"method": "item/started", "params": {"threadId": thread, "turnId": turn, "item": {"type": "commandExecution", "id": "item-1", "command": "cargo test"}}})
        out({"id": 7000 + mid, "method": "item/commandExecution/requestApproval", "params": {"threadId": thread, "turnId": turn, "itemId": "item-1", "command": "cargo test", "cwd": os.getcwd(), "reason": "runs the tests"}})
    elif meth is None and mid is not None:
        L("decision:%s:%s" % (mid, m.get("result", {}).get("decision")))
        time.sleep(float(os.environ.get("VKFAKE_TURN_SECS", "0")))
        out({"method": "item/completed", "params": {"threadId": thread, "item": {"type": "commandExecution", "id": "item-1", "status": "completed", "exitCode": 0}}})
        out({"method": "item/completed", "params": {"threadId": thread, "item": {"type": "agentMessage", "id": "item-2", "text": "All green."}}})
        out({"method": "thread/tokenUsage/updated", "params": {"threadId": thread, "tokenUsage": {"total": {"inputTokens": 100, "cachedInputTokens": 10, "outputTokens": 20, "reasoningOutputTokens": 0}}}})
        out({"method": "turn/completed", "params": {"threadId": thread, "turn": {"id": turn, "status": "completed"}}})
        busy = False
        L("turn_done")
    elif meth == "turn/interrupt":
        L("interrupt")
"#;

/// `claude -p --input-format stream-json --output-format stream-json` per 04 §6.1.3: system/init
/// on the first user message, a tool use that needs permission (`can_use_tool` control request),
/// the result after the `control_response`.
const FAKE_CLAUDE: &str = r#"
import json, os, sys
log = open(os.environ["VKFAKE_LOG"], "a")
def L(s): log.write(s + "\n"); log.flush()
def out(o): sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
L("start:claude:%d:%s" % (os.getpid(), " ".join(sys.argv[1:])))
L("env:CLAUDE_CONFIG_DIR=" + os.environ.get("CLAUDE_CONFIG_DIR", ""))
a = sys.argv
sid = a[a.index("--session-id") + 1] if "--session-id" in a else a[a.index("--resume") + 1]
for line in sys.stdin:
    m = json.loads(line)
    t = m.get("type")
    if t == "control_request":
        sub = m["request"]["subtype"]
        L("control:" + sub)
        out({"type": "control_response", "response": {"subtype": "success", "request_id": m["request_id"], "response": {}}})
    elif t == "user":
        text = m["message"]["content"][0]["text"]
        L("prompt:" + text)
        out({"type": "system", "subtype": "init", "session_id": sid, "model": "fake-model", "cwd": os.getcwd(), "permissionMode": "default"})
        out({"type": "assistant", "session_id": sid, "message": {"content": [{"type": "text", "text": "Writing the file."}, {"type": "tool_use", "id": "tu1", "name": "Write", "input": {"file_path": "notes.txt", "content": "x"}}]}})
        out({"type": "control_request", "request_id": "perm-1", "request": {"subtype": "can_use_tool", "tool_name": "Write", "input": {"file_path": "notes.txt", "content": "x"}, "tool_use_id": "tu1"}})
    elif t == "control_response":
        r = m["response"]
        L("perm:%s:%s" % (r["request_id"], r["response"]["behavior"]))
        out({"type": "user", "session_id": sid, "message": {"content": [{"type": "tool_result", "tool_use_id": "tu1", "is_error": False}]}})
        out({"type": "result", "subtype": "success", "is_error": False, "result": "Done.", "session_id": sid, "total_cost_usd": 0.5, "usage": {"input_tokens": 30, "output_tokens": 4, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}})
"#;

/// `pi --mode rpc` per 04 §6.3: get_state, prompt, an extension confirm dialog
/// (`extension_ui_request`), tools, agent_end; the prompt "close stdin" makes it close stdin
/// (so a later write fails and is reported as `input_unconfirmed`).
const FAKE_PI: &str = r#"
import json, os, sys, time
log = open(os.environ["VKFAKE_LOG"], "a")
def L(s): log.write(s + "\n"); log.flush()
def out(o): sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
L("start:pi:%d:%s" % (os.getpid(), " ".join(sys.argv[1:])))
busy = False
while True:
    line = sys.stdin.readline()
    if not line:
        break
    m = json.loads(line)
    t, mid = m.get("type"), m.get("id")
    if t == "get_state":
        out({"type": "response", "id": mid, "command": "get_state", "success": True, "data": {"sessionId": "pi-fake", "sessionFile": "/tmp/pi-fake.jsonl", "isStreaming": busy}})
    elif t == "prompt":
        text = m["message"]; L("prompt:" + text)
        out({"type": "response", "id": mid, "command": "prompt", "success": True})
        out({"type": "agent_start"})
        if text == "close stdin":
            out({"type": "agent_end", "messages": []})
            os.close(0)
            L("stdin closed")
            time.sleep(60)
            break
        busy = True
        out({"type": "extension_ui_request", "id": "ui-1", "method": "confirm", "title": "Allow bash?", "message": "rm -rf target"})
    elif t == "extension_ui_response":
        L("ui:%s:%s" % (m["id"], m.get("confirmed")))
        out({"type": "tool_execution_start", "toolCallId": "c1", "toolName": "bash", "args": {"command": "ls"}})
        out({"type": "tool_execution_end", "toolCallId": "c1", "toolName": "bash", "result": {}, "isError": False})
        out({"type": "message_end", "message": {"role": "assistant", "content": [{"type": "text", "text": "Skipped the delete."}]}})
        out({"type": "turn_end", "message": {"usage": {"input": 11, "output": 3, "cacheRead": 0, "cacheWrite": 0, "cost": {"total": 0.02}}}})
        out({"type": "agent_end", "messages": []})
        busy = False
"#;

/// An ACP agent per 04 §6.6 with `loadSession`: `session/load` replays the conversation.
const FAKE_ACP: &str = r#"
import json, os, sys
log = open(os.environ["VKFAKE_LOG"], "a")
def L(s): log.write(s + "\n"); log.flush()
def out(o): sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
L("start:acp:%d:%s" % (os.getpid(), " ".join(sys.argv[1:])))
def upd(sid, u): out({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": u}})
for line in sys.stdin:
    m = json.loads(line)
    meth, mid = m.get("method"), m.get("id")
    if meth == "initialize":
        out({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}}})
    elif meth == "session/new":
        out({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": "acp-fake-1"}})
    elif meth == "session/load":
        sid = m["params"]["sessionId"]; L("load:" + sid)
        upd(sid, {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "earlier question"}})
        upd(sid, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "earlier answer"}})
        out({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif meth == "session/prompt":
        sid = m["params"]["sessionId"]
        L("prompt:" + m["params"]["prompt"][0]["text"])
        upd(sid, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hello from ACP."}})
        out({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn", "usage": {"inputTokens": 9, "outputTokens": 2}}})
"#;

#[test]
fn codex_headless_run_survives_server_kill_mid_turn_and_completes_once() {
    let Some(py) = python3() else {
        eprintln!("skipping: no python3 for the fake harnesses");
        return;
    };
    let s = Session::new(&py, "3");
    let pane = s.workspace_pane();
    let started = s
        .api(
            "agent.start",
            json!({"pane": pane, "harness": "codex", "mode": "headless", "name": "cx"}),
        )
        .unwrap();
    let run_id = started["run"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        started["run"]["integration"], "headless:app-server",
        "{started}"
    );
    let run = s.until("codex thread open", 20, || {
        let r = s.run(&run_id);
        (r["harness_session_id"] == "th-fake-1" && r["execution"]["value"] == "Idle").then_some(r)
    });
    assert!(
        run["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("survive_disconnect"))
    );
    let child = s.harness_pid("codex");
    assert!(s.log_text().contains(&format!(
        "env:CODEX_HOME={}",
        s.dir.path().join("codex-home").display()
    )));
    assert!(s.log_text().contains("app-server"), "{}", s.log_text());
    let holder = parent_pid(child);
    assert!(holder > 1);

    // A dropped holder connection (no restart) reconnects without losing the stream.
    // SAFETY: SIGUSR1 is the holder's connection-drop chaos hook.
    unsafe { libc::kill(holder, libc::SIGUSR1) };
    std::thread::sleep(Duration::from_millis(300));

    s.api(
        "agent.prompt",
        json!({"target": run_id, "text": "run the tests"}),
    )
    .unwrap();
    let it = s.until("approval interaction", 20, || {
        s.open_interactions(&run_id).into_iter().next()
    });
    assert!(
        it["native_ref"].as_str().unwrap().starts_with("rpc:"),
        "{it}"
    );
    assert_eq!(it["answer_channel"], "Native");
    assert_eq!(it["action"]["command"], "cargo test");
    s.api(
        "interaction.answer",
        json!({"interaction": it["id"], "decision": "allow"}),
    )
    .unwrap();
    s.until("decision reached the harness", 10, || {
        s.log_text().contains(":accept").then_some(())
    });

    // kill -9 mid-turn: the turn finishes while no server is attached.
    s.kill_server();
    assert!(alive(child), "the harness survives in its pipe-mode holder");
    s.until("turn finished without a server", 10, || {
        s.log_text().contains("turn_done").then_some(())
    });

    // Restart (any call starts the server), journal replay, reconcile.
    let r = s.until("turn completed after restart", 30, || {
        let r = s.run(&run_id);
        (r["execution"]["value"] == "Idle" && r["last_message"] == "All green.").then_some(r)
    });
    assert!(alive(child));
    assert_eq!(s.harness_pid("codex"), child, "same process");
    assert_eq!(s.log_count("start:codex"), 1, "never respawned");
    assert_eq!(
        s.log_count("decision:"),
        1,
        "the decision was delivered exactly once"
    );
    assert_eq!(r["turns_completed"], 1, "{r}");
    assert_eq!(r["usage"]["input_tokens"], 100);
    let done: Vec<Value> = s
        .events("agent.turn_completed")
        .into_iter()
        .filter(|e| e["subject"]["run"] == run_id)
        .collect();
    assert_eq!(done.len(), 1, "no duplicated turn: {done:?}");
    let tools: Vec<Value> = s
        .events("agent.state_changed")
        .into_iter()
        .filter(|e| e["subject"]["run"] == run_id && e["data"]["to"] == "working")
        .collect();
    assert_eq!(tools.len(), 1, "one working transition: {tools:?}");
    s.until("reconcile query after the restart", 10, || {
        s.log_text().contains("thread/read").then_some(())
    });
    let it = s
        .api("interaction.get", json!({"interaction": it["id"]}))
        .unwrap();
    assert_eq!(it["interaction"]["delivery"], "Delivered", "{it}");
    let recovered: Vec<Value> = s
        .events("pane.recovered")
        .into_iter()
        .filter(|e| e["data"]["method"] == "journal")
        .collect();
    assert!(!recovered.is_empty(), "recovered by journal replay");
    // The transcript was rebuilt from the journal.
    let screen = s.json(&["pane", "read", started["pane"].as_str().unwrap()]);
    assert!(screen.to_string().contains("All green."), "{screen}");
}

#[test]
fn claude_headless_pending_approval_survives_restart_and_is_answered_once() {
    let Some(py) = python3() else {
        eprintln!("skipping: no python3 for the fake harnesses");
        return;
    };
    let s = Session::new(&py, "0");
    let pane = s.workspace_pane();
    let started = s
        .api(
            "agent.start",
            json!({"pane": pane, "harness": "claude", "mode": "headless", "prompt": "write notes"}),
        )
        .unwrap();
    let run_id = started["run"]["id"].as_str().unwrap().to_string();
    assert_eq!(started["run"]["integration"], "headless:stream-json");
    let sid = started["run"]["harness_session_id"]
        .as_str()
        .expect("pre-assigned session id")
        .to_string();
    let it = s.until("permission request", 20, || {
        s.open_interactions(&run_id).into_iter().next()
    });
    assert_eq!(it["native_ref"], "rpc:perm-1");
    assert_eq!(it["action"]["tool"], "Write");
    let log = s.log_text();
    assert!(log.contains(&format!("--session-id {sid}")), "{log}");
    assert!(log.contains("--input-format stream-json"), "{log}");
    assert!(log.contains("--permission-prompt-tool stdio"), "{log}");
    assert!(log.contains("prompt:write notes"));
    let child = s.harness_pid("claude");

    s.kill_server();
    assert!(alive(child));
    // After the restart the same request is still the one open interaction (re-attached by
    // native ref, not duplicated).
    let open = s.until("interaction after restart", 30, || {
        let v = s.open_interactions(&run_id);
        (!v.is_empty()).then_some(v)
    });
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0]["id"], it["id"]);
    s.api(
        "interaction.answer",
        json!({"interaction": it["id"], "decision": "allow"}),
    )
    .unwrap();
    let r = s.until("turn done", 20, || {
        let r = s.run(&run_id);
        (r["execution"]["value"] == "Idle" && r["last_message"] == "Done.").then_some(r)
    });
    assert_eq!(s.log_count("perm:perm-1:allow"), 1, "{}", s.log_text());
    assert_eq!(r["usage"]["input_tokens"], 30);
    assert_eq!(r["harness_session_id"], sid.as_str());
    assert_eq!(s.log_count("start:claude"), 1);
    let it = s
        .api("interaction.get", json!({"interaction": it["id"]}))
        .unwrap();
    assert_eq!(it["interaction"]["delivery"], "Delivered");
}

#[test]
fn pi_headless_dialog_answer_and_input_unconfirmed_report() {
    let Some(py) = python3() else {
        eprintln!("skipping: no python3 for the fake harnesses");
        return;
    };
    let s = Session::new(&py, "0");
    let pane = s.workspace_pane();
    let started = s
        .api(
            "agent.start",
            json!({"pane": pane, "harness": "pi", "mode": "headless"}),
        )
        .unwrap();
    let run_id = started["run"]["id"].as_str().unwrap().to_string();
    assert_eq!(started["run"]["integration"], "headless:rpc");
    s.until("pi session", 20, || {
        let r = s.run(&run_id);
        (r["harness_session_id"] == "pi-fake" && r["execution"]["value"] == "Idle").then_some(())
    });
    assert!(s.log_text().contains("--mode rpc"), "{}", s.log_text());
    s.api(
        "agent.prompt",
        json!({"target": run_id, "text": "clean up"}),
    )
    .unwrap();
    let it = s.until("extension dialog", 20, || {
        s.open_interactions(&run_id).into_iter().next()
    });
    assert_eq!(it["kind"], "approval");
    assert_eq!(it["native_ref"], "rpc:ui-1");
    s.api(
        "interaction.answer",
        json!({"interaction": it["id"], "decision": "deny"}),
    )
    .unwrap();
    let r = s.until("turn done", 20, || {
        let r = s.run(&run_id);
        (r["execution"]["value"] == "Idle" && r["last_message"] == "Skipped the delete.")
            .then_some(r)
    });
    assert_eq!(s.log_count("ui:ui-1:False"), 1);
    assert_eq!(r["usage"]["output_tokens"], 3);

    // The harness closes its stdin; a later prompt cannot be written and is reported as
    // input_unconfirmed (never silently dropped, never replayed).
    s.api(
        "agent.prompt",
        json!({"target": run_id, "text": "close stdin"}),
    )
    .unwrap();
    s.until("stdin closed", 10, || {
        s.log_text().contains("stdin closed").then_some(())
    });
    s.api(
        "agent.prompt",
        json!({"target": run_id, "text": "lost words"}),
    )
    .unwrap();
    let ev = s.until("input_unconfirmed event", 10, || {
        s.events("pane.input_unconfirmed").into_iter().next()
    });
    assert_eq!(ev["subject"]["run"], run_id);
    let screen = s.json(&["pane", "read", started["pane"].as_str().unwrap()]);
    assert!(screen.to_string().contains("not confirmed"), "{screen}");
}

#[test]
fn acp_headless_run_resumes_with_session_load() {
    let Some(py) = python3() else {
        eprintln!("skipping: no python3 for the fake harnesses");
        return;
    };
    let s = Session::new(&py, "0");
    let pane = s.workspace_pane();
    let agent = s.fakebin.join("fake-acp");
    let started = s
        .api(
            "agent.start",
            json!({"pane": pane, "acp": agent.to_string_lossy(), "mode": "headless", "name": "acpy"}),
        )
        .unwrap();
    let run_id = started["run"]["id"].as_str().unwrap().to_string();
    assert_eq!(started["run"]["integration"], "headless:acp", "{started}");
    s.until("ACP session", 20, || {
        let r = s.run(&run_id);
        (r["harness_session_id"] == "acp-fake-1" && r["execution"]["value"] == "Idle").then_some(())
    });
    s.api(
        "agent.prompt",
        json!({"target": run_id, "text": "hi", "wait": true}),
    )
    .unwrap();
    let r = s.run(&run_id);
    assert_eq!(r["last_message"], "Hello from ACP.", "{r}");
    assert_eq!(r["usage"]["input_tokens"], 9);

    // The agent process dies; resuming starts a new one that loads the session.
    let child = s.harness_pid("acp");
    // SAFETY: killing the fake agent this test started.
    unsafe { libc::kill(child, libc::SIGKILL) };
    s.until("run ended", 10, || {
        s.live_run(&run_id).is_none().then_some(())
    });
    let ended: Vec<Value> = s
        .events("agent.exited")
        .into_iter()
        .filter(|e| e["subject"]["run"] == run_id)
        .collect();
    assert_eq!(ended.len(), 1, "{ended:?}");
    let resumed = s.api("agent.resume", json!({"run": run_id})).unwrap();
    let new_id = resumed["run"]["id"].as_str().unwrap().to_string();
    assert_ne!(new_id, run_id);
    s.until("session loaded", 20, || {
        let r = s.run(&new_id);
        (r["harness_session_id"] == "acp-fake-1" && r["execution"]["value"] == "Idle").then_some(())
    });
    assert_eq!(s.log_count("load:acp-fake-1"), 1, "{}", s.log_text());
    assert_eq!(s.log_count("start:acp"), 2);
    let screen = s.json(&["pane", "read", resumed["pane"].as_str().unwrap()]);
    assert!(screen.to_string().contains("earlier answer"), "{screen}");
}

/// An ACP agent that uses `terminal/*` on its first prompt (04 §6.6): a command in a cwd outside
/// the session cwd (refused), then `echo term-ok; exit 3` in the session cwd: wait for exit,
/// read the output, release. Its answer reports what it saw.
const FAKE_ACP_TERM: &str = r#"
import json, os, sys
log = open(os.environ["VKFAKE_LOG"], "a")
def L(s): log.write(s + "\n"); log.flush()
def out(o): sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
L("start:acpterm:%d" % os.getpid())
nid = [100]
def call(method, params):
    nid[0] += 1; i = nid[0]
    out({"jsonrpc": "2.0", "id": i, "method": method, "params": params})
    for line in sys.stdin:
        m = json.loads(line)
        if m.get("id") == i and "method" not in m:
            return m
caps = None
for line in sys.stdin:
    m = json.loads(line)
    meth, mid = m.get("method"), m.get("id")
    if meth == "initialize":
        caps = m["params"]["clientCapabilities"]; L("caps:terminal=%s" % caps.get("terminal"))
        out({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif meth == "session/new":
        cwd = m["params"]["cwd"]
        out({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": "term-1"}})
    elif meth == "session/prompt":
        sid = m["params"]["sessionId"]
        bad = call("terminal/create", {"sessionId": sid, "command": "ls", "cwd": "/"})
        L("outside:%s" % bad.get("error", {}).get("code"))
        r = call("terminal/create", {"sessionId": sid, "command": "echo term-ok; exit 3", "cwd": cwd, "outputByteLimit": 4096})
        tid = r["result"]["terminalId"]
        ex = call("terminal/wait_for_exit", {"sessionId": sid, "terminalId": tid})["result"]
        o = call("terminal/output", {"sessionId": sid, "terminalId": tid})["result"]
        call("terminal/release", {"sessionId": sid, "terminalId": tid})
        msg = "exit=%s out=%s" % (ex.get("exitCode"), "term-ok" in o.get("output", ""))
        L(msg)
        out({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": msg}}}})
        out({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
"#;

/// ACP `terminal/*` end to end: the terminal is a real pane, confined to the session cwd; its
/// exit status and output reach the agent.
#[test]
fn acp_headless_terminals_run_as_panes() {
    let Some(py) = python3() else {
        eprintln!("skipping: no python3 for the fake harnesses");
        return;
    };
    let s = Session::new(&py, "0");
    let agent = s.fakebin.join("fake-acp-term");
    std::fs::write(&agent, format!("#!{py}\n{FAKE_ACP_TERM}")).unwrap();
    std::fs::set_permissions(&agent, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let pane = s.workspace_pane();
    let started = s
        .api(
            "agent.start",
            json!({"pane": pane, "acp": agent.to_string_lossy(), "mode": "headless"}),
        )
        .unwrap();
    let run_id = started["run"]["id"].as_str().unwrap().to_string();
    s.until("ACP session", 20, || {
        let r = s.run(&run_id);
        (r["harness_session_id"] == "term-1" && r["execution"]["value"] == "Idle").then_some(())
    });
    assert_eq!(s.log_count("caps:terminal=True"), 1, "{}", s.log_text());
    // Immediate exits must retain both the final screen and status, even when the
    // watcher is scheduled after the pane has already left the runtime map.
    for turn in 1..=10 {
        if let Err(e) = s.api(
            "agent.prompt",
            json!({"target": run_id, "text": "use a terminal", "wait": true, "timeout_ms": 30000}),
        ) {
            let logs = std::fs::read_dir(s.dir.path().join("state/default/logs"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|f| {
                    let t = std::fs::read_to_string(f.path()).unwrap_or_default();
                    let mut from = t.len().saturating_sub(6000);
                    while !t.is_char_boundary(from) {
                        from += 1;
                    }
                    format!("== {}\n{}", f.path().display(), &t[from..])
                })
                .collect::<Vec<_>>()
                .join("\n");
            let panes = s.cmd(&["pane", "list"]).output().unwrap();
            panic!(
                "turn {turn}: {e}\nfake log:\n{}\npanes: {}\n{logs}",
                s.log_text(),
                String::from_utf8_lossy(&panes.stdout)
            );
        }
        assert_eq!(s.log_count("outside:-32002"), turn, "{}", s.log_text());
        let r = s.run(&run_id);
        assert_eq!(
            r["last_message"],
            "exit=3 out=True",
            "{r}\n{}",
            s.log_text()
        );
    }
}

/// `agents.harness.codex.headless_shared = true` (04 §6.2): two headless Codex runs share one
/// app-server (one process, one `initialized`), each with its own thread; an approval reaches
/// only the run whose thread asked.
#[test]
fn codex_headless_shared_app_server_multiplexes_runs() {
    let Some(py) = python3() else {
        eprintln!("skipping: no python3 for the fake harnesses");
        return;
    };
    let s = Session::new(&py, "0");
    std::fs::write(
        s.dir.path().join("config.toml"),
        "[agents.harness.codex]\nheadless_shared = true\n",
    )
    .unwrap();
    let pane = s.workspace_pane();
    let start = |name: &str, thread: &str| -> String {
        let started = s
            .api(
                "agent.start",
                json!({"pane": pane, "harness": "codex", "mode": "headless", "name": name}),
            )
            .unwrap();
        let id = started["run"]["id"].as_str().unwrap().to_string();
        s.until("codex thread open", 20, || {
            let r = s.run(&id);
            (r["harness_session_id"] == thread && r["execution"]["value"] == "Idle").then_some(())
        });
        id
    };
    let a = start("cxa", "th-fake-1");
    let b = start("cxb", "th-fake-2");
    assert_eq!(
        s.log_count("start:codex"),
        1,
        "one app-server: {}",
        s.log_text()
    );
    assert_eq!(s.log_count("initialized"), 1);

    s.api("agent.prompt", json!({"target": b, "text": "on b"}))
        .unwrap();
    let it = s.until("approval on b", 20, || {
        s.open_interactions(&b).into_iter().next()
    });
    assert!(s.open_interactions(&a).is_empty());
    s.api(
        "interaction.answer",
        json!({"interaction": it["id"], "decision": "allow"}),
    )
    .unwrap();
    s.until("turn done on b", 20, || {
        (s.run(&b)["last_message"] == "All green.").then_some(())
    });
    assert!(s.run(&a)["last_message"].is_null(), "{}", s.run(&a));
}

/// An ACP agent that probes its confinement on each prompt: reads `$PROBE_SECRET` and connects
/// to `127.0.0.1:$PROBE_PORT`, then reports both outcomes as its answer. It writes no log file
/// (a sandboxed agent cannot write outside its checkout).
#[cfg(target_os = "macos")]
const FAKE_PROBE: &str = r#"
import json, os, socket, sys
def out(o): sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
def probe():
    try:
        open(os.environ["PROBE_SECRET"]).read(); r = "read=ok"
    except Exception:
        r = "read=denied"
    try:
        s = socket.create_connection(("127.0.0.1", int(os.environ["PROBE_PORT"])), timeout=3); s.close(); n = "net=ok"
    except Exception:
        n = "net=denied"
    return r + " " + n
for line in sys.stdin:
    m = json.loads(line)
    meth, mid = m.get("method"), m.get("id")
    if meth == "initialize":
        out({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {}}})
    elif meth == "session/new":
        out({"jsonrpc": "2.0", "id": mid, "result": {"sessionId": "probe-1"}})
    elif meth == "session/prompt":
        sid = m["params"]["sessionId"]
        out({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": probe()}}}})
        out({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
"#;

/// Review finding 1: `agent.start {mode: "headless", isolate, network}` goes through the same
/// isolation path as PTY agents. The same probe agent reads a host secret and reaches a local
/// listener when run on the host, and is refused both when started with `isolate: "sandbox",
/// network: "none"`. A network profile without an isolation level is refused outright instead
/// of silently running on the host.
#[cfg(target_os = "macos")]
#[test]
fn headless_isolation_confines_the_harness() {
    let Some(py) = python3() else {
        eprintln!("skipping: no python3 for the fake harnesses");
        return;
    };
    let s = Session::new(&py, "0");
    // Inside the checkout: the sandbox hides the rest of the temp dir (and /tmp) from reads.
    let probe = s.dir.path().join("work/fake-probe");
    std::fs::write(&probe, format!("#!{py}\n{FAKE_PROBE}")).unwrap();
    std::fs::set_permissions(&probe, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let secret = s.dir.path().join("home/secret.txt");
    std::fs::write(&secret, "TOP SECRET").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let pane = s.workspace_pane();
    let env = json!({"PROBE_SECRET": secret.to_string_lossy(), "PROBE_PORT": port.to_string()});

    let answer = |extra: Value| -> String {
        let mut p =
            json!({"pane": pane, "acp": probe.to_string_lossy(), "mode": "headless", "env": env});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        let started = s.api("agent.start", p).unwrap();
        let Some(run_id) = started["run"]["id"].as_str().map(str::to_string) else {
            panic!("no run ({extra}): {started}");
        };
        s.until("probe session", 30, || {
            let r = s.run(&run_id);
            (r["harness_session_id"] == "probe-1" && r["execution"]["value"] == "Idle")
                .then_some(())
        });
        s.api(
            "agent.prompt",
            json!({"target": run_id, "text": "probe", "wait": true, "timeout_ms": 30000}),
        )
        .unwrap();
        s.run(&run_id)["last_message"]
            .as_str()
            .unwrap_or("")
            .to_string()
    };

    // Control: on the host the probe succeeds, so the refusals below are the sandbox's.
    assert_eq!(answer(json!({})), "read=ok net=ok");
    let confined = answer(json!({"isolate": "sandbox", "network": "none"}));
    assert_eq!(confined, "read=denied net=denied");

    let e = s
        .api(
            "agent.start",
            json!({"pane": pane, "acp": probe.to_string_lossy(), "mode": "headless", "network": "none"}),
        )
        .expect_err("network without isolation must be refused");
    assert!(e.to_string().contains("isolation level"), "{e}");
    drop(listener);
}
