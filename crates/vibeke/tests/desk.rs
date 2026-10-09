//! Session desk (R2) and drafts composer (R3) end to end with a fake Claude that reports
//! through the real hook shim and writes a fixture transcript in a temp dir: the desk finds the
//! live session; drafts are sent through the guarded prompt-input path (delivered only on a
//! matching turn, zero bytes when the input box has a user draft); pane tokens can't send.

use serde_json::{Value, json};
use std::process::Command;
use std::time::{Duration, Instant};

const FAKE_CLAUDE: &str = r#"#!/bin/sh
# Minimal stand-in for an interactive harness: an input box, prompts reported via hooks and
# appended to a Claude-format transcript.
SID=desk-sess-1
T=__TRANSCRIPT__
h() { printf '%s' "$2" | "$VIBEKE_BIN" hook claude "$1" >/dev/null 2>&1; }
q() { python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'; }
h SessionStart "{\"session_id\":\"$SID\",\"source\":\"startup\",\"transcript_path\":\"$T\"}"
while true; do
  printf '\n╭──────────────────╮\n│ > '
  IFS= read -r line || exit 0
  P=$(printf '%s' "$line" | q)
  printf '{"type":"user","sessionId":"%s","cwd":"/tmp","message":{"content":%s}}\n' "$SID" "$P" >> "$T"
  h UserPromptSubmit "{\"session_id\":\"$SID\",\"prompt\":$P}"
  echo "working on: $line"
  printf '{"type":"assistant","sessionId":"%s","message":{"content":[{"type":"text","text":"Done with that request."}]}}\n' "$SID" >> "$T"
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
                .prefix("vkdesk")
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
    fn try_json(&self, args: &[&str]) -> Result<Value, Value> {
        let out = self.cmd(args).output().unwrap();
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
    fn shell_pane(&self) -> String {
        let pane = self.json(&["workspace", "create", "--cwd", "/tmp"])["root_pane"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let _ = self
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
        pane
    }
    fn wait_sent(&self, draft: &str) -> Value {
        self.until("send outcome", 20, || {
            let d = self.json(&["draft", "show", draft]);
            let st = d["draft"]["sends"].as_array()?.last()?["state"]
                .as_str()?
                .to_string();
            (st != "sending").then_some(d)
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

#[test]
fn desk_finds_live_session_and_drafts_send_safely() {
    let s = Session::new();
    let transcript = s.dir.path().join("transcripts").join("desk-sess-1.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let script = s.dir.path().join("fake-claude");
    std::fs::write(
        &script,
        FAKE_CLAUDE.replace("__TRANSCRIPT__", &transcript.to_string_lossy()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let pane = s.shell_pane();
    s.json(&["pane", "run", &pane, &script.to_string_lossy()]);
    let run = s.until("hook-bound run", 15, || {
        let v = s.json(&["agent", "list"]);
        v["runs"].as_array()?.iter().find_map(|r| {
            (r["harness_session_id"] == "desk-sess-1" && r["transcript_path"].is_string())
                .then(|| r["id"].as_str().unwrap().to_string())
        })
    });

    // The user works directly in the harness; the desk finds it and knows it is live.
    s.json(&["pane", "run", &pane, "Fix the flaky websocket reconnect"]);
    let hit = s.until("indexed turn", 15, || {
        let v = s.json(&["desk", "search", "websocket", "reconnect"]);
        v["hits"].as_array()?.first().cloned()
    });
    assert_eq!(hit["session"], "desk-sess-1");
    assert_eq!(hit["turn"], 1);
    assert_eq!(hit["status"], "live", "{hit}");
    assert_eq!(hit["live"]["run"], run.as_str());
    let open = s.json(&["desk", "open", "desk-sess-1", "--turn", "1"]);
    assert_eq!(open["action"], "focus_live_pane");
    assert_eq!(open["focused"], false);
    let sessions = s.json(&["desk", "sessions", "--harness", "claude"]);
    assert_eq!(sessions["sessions"][0]["session"], "desk-sess-1");

    // The run's title: the first prompt, then the title Claude generates, then the user's.
    let title = |want: &str| {
        s.until(&format!("run title {want:?}"), 15, || {
            let v = s.json(&["agent", "get", &run]);
            let r = if v["run"].is_object() { &v["run"] } else { &v };
            (r["title"] == want).then_some(())
        })
    };
    title("Fix the flaky websocket reconnect");
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    use std::io::Write;
    writeln!(
        f,
        r#"{{"type":"ai-title","aiTitle":"Websocket reconnect fix","sessionId":"desk-sess-1"}}"#
    )
    .unwrap();
    title("Websocket reconnect fix");
    writeln!(
        f,
        r#"{{"type":"custom-title","customTitle":"reconnect","sessionId":"desk-sess-1"}}"#
    )
    .unwrap();
    title("reconnect");

    // Drafts: kept outside the agent input, sent only through the guarded path.
    let ws = s.json(&["pane", "get", &pane])["pane"]["workspace"]
        .as_str()
        .unwrap()
        .to_string();
    let d1 = s.json(&[
        "draft",
        "new",
        "Please add a regression test",
        "--workspace",
        &ws,
    ]);
    let id1 = d1["draft"]["id"].as_str().unwrap().to_string();
    let cap = s.json(&["draft", "check", &id1, "--run", &run]);
    assert_eq!(cap["send_path"], "prompt_input", "{cap}");
    let sent = s.json(&[
        "draft",
        "send",
        &id1,
        "--run",
        &run,
        "--idempotency-key",
        "send-1",
    ]);
    assert_eq!(sent["send"]["state"], "sending");
    let done = s.wait_sent(&id1);
    let last = done["draft"]["sends"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["state"], "delivered", "{done}");
    assert_eq!(
        done["draft"]["archived"], true,
        "a delivered draft is archived"
    );
    // Same key: the outcome is reported, nothing is sent again.
    let again = s.json(&[
        "draft",
        "send",
        &id1,
        "--run",
        &run,
        "--idempotency-key",
        "send-1",
    ]);
    assert_eq!(again["replayed"], true);
    // The delivered prompt reached the transcript (and the desk).
    s.until("delivered prompt indexed", 15, || {
        let v = s.json(&["desk", "search", "regression", "test"]);
        v["hits"]
            .as_array()?
            .iter()
            .any(|h| h["turn"] == 2)
            .then_some(())
    });

    // Notes are sent only when included.
    s.json(&["notes", "set", "use the staging db", "--workspace", &ws]);
    let d2 = s.json(&["draft", "new", "Run the suite", "--workspace", &ws]);
    let id2 = d2["draft"]["id"].as_str().unwrap().to_string();
    s.json(&[
        "draft",
        "send",
        &id2,
        "--run",
        &run,
        "--include-notes",
        "--keep",
    ]);
    let done = s.wait_sent(&id2);
    let last = done["draft"]["sends"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["state"], "delivered", "{done}");
    assert!(
        last["text"]
            .as_str()
            .unwrap()
            .contains("use the staging db")
    );
    assert_eq!(done["draft"]["archived"], false, "--keep keeps it");

    // Unsafe: the user has half-typed text in the agent's input → zero bytes, draft kept.
    let d3 = s.json(&["draft", "new", "Second thought", "--workspace", &ws]);
    let id3 = d3["draft"]["id"].as_str().unwrap().to_string();
    let _ = s.cmd(&["pane", "send-text", &pane, "half typed"]).output();
    std::thread::sleep(Duration::from_millis(300));
    let e = s
        .try_json(&["draft", "send", &id3, "--run", &run])
        .unwrap_err();
    assert_eq!(e["error"]["details"]["reason"], "send_unsafe", "{e}");
    assert_eq!(e["error"]["details"]["fallback"], "open_pane_to_send");
    let kept = s.json(&["draft", "show", &id3]);
    assert!(kept["draft"]["sends"].as_array().unwrap().is_empty());
    let screen = s.json(&["pane", "read", &pane, "--lines", "5"])["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(!screen.contains("Second thought"), "zero bytes: {screen}");
    let _ = s.cmd(&["pane", "send-keys", &pane, "ctrl+u"]).output();
    let list = s.json(&["draft", "list", "--workspace", &ws]);
    let ids: Vec<&str> = list["drafts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![id2.as_str(), id3.as_str()]);

    // A pane token can't send drafts or resume sessions.
    let other = s.shell_pane();
    s.json(&["pane", "run", &other, &format!("$VIBEKE_BIN --json api call draft.send '{{\"draft\":\"{id3}\",\"target_run\":\"{run}\",\"idempotency_key\":\"x\"}}' 2>&1 | head -c 300; $VIBEKE_BIN --json api call desk.resume '{{\"session\":\"desk-sess-1\"}}' 2>&1 | head -c 300; echo; echo scope-done")]);
    let _ = s
        .cmd(&[
            "pane",
            "wait-output",
            &other,
            "--regex",
            "(?m)^scope-done$",
            "--timeout-ms",
            "10000",
        ])
        .output();
    let txt = s.json(&[
        "pane", "read", &other, "--source", "recent", "--lines", "20",
    ])["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert_eq!(txt.matches("permission_denied").count(), 2, "{txt}");

    // Forget purges the session from the index.
    let f = s.json(&["desk", "forget", "desk-sess-1"]);
    assert!(f["rows_deleted"].as_u64().unwrap() >= 3, "{f}");
    let v = s.json(&["desk", "search", "websocket"]);
    assert!(v["hits"].as_array().unwrap().is_empty());
}
