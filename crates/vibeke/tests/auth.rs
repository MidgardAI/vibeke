//! Authorization matrix (09 §5.2): processes inside panes get pane scope (by token or by process
//! ancestry), can drive their own pane and panes they created, and can't answer interactions,
//! type into other panes or control the server.

use serde_json::Value;
use std::process::Command;
use std::time::Duration;

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new() -> Self {
        Session {
            dir: tempfile::Builder::new()
                .prefix("vkauth")
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
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    /// Run a shell line inside `pane` and return its output tail.
    fn in_pane(&self, pane: &str, line: &str) -> String {
        let marker = format!("done-{}", rand_tag());
        let full = format!("{line}; echo {marker}");
        let _ = self.json(&["pane", "run", pane, &full]);
        let _ = self
            .cmd(&[
                "pane",
                "wait-output",
                pane,
                "--regex",
                &format!("(?m)^{marker}$"),
                "--timeout-ms",
                "15000",
            ])
            .output();
        std::thread::sleep(Duration::from_millis(200));
        let text =
            self.json(&["pane", "read", pane, "--source", "recent", "--lines", "40"])["text"]
                .as_str()
                .unwrap_or("")
                .to_string();
        if !text.contains(&format!("\n{marker}")) {
            let log =
                std::fs::read_to_string(self.dir.path().join("state/default/logs/server.log"))
                    .unwrap_or_default();
            eprintln!(
                "--- pane text:\n{text}\n--- server log tail:\n{}",
                log.lines().rev().take(15).collect::<Vec<_>>().join("\n")
            );
        }
        text
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn rand_tag() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
}

#[test]
fn pane_scope_matrix() {
    let s = Session::new();
    let a = s.json(&["workspace", "create", "--cwd", "/tmp"])["root_pane"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b = s.json(&["pane", "split", &a, "--direction", "right"])["pane"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    // Let both login shells reach their prompt.
    for p in [&a, &b] {
        let _ = s
            .cmd(&[
                "pane",
                "wait-idle",
                p,
                "--quiet-ms",
                "700",
                "--timeout-ms",
                "10000",
            ])
            .output();
    }

    // From pane A: typing into pane B (not created by A) is refused.
    let out = s.in_pane(
        &a,
        &format!("$VIBEKE_BIN pane send-text {b} intruder 2>&1 | head -c 200"),
    );
    assert!(
        out.contains("permission_denied"),
        "send-text to another pane must be denied: {out}"
    );
    // Without the token (stripping it) the process is still scoped by ancestry.
    let out = s.in_pane(
        &a,
        &format!(
            "env -u VIBEKE_PANE_TOKEN $VIBEKE_BIN pane send-text {b} intruder 2>&1 | head -c 200"
        ),
    );
    assert!(
        out.contains("permission_denied"),
        "ancestry must scope token-less callers: {out}"
    );
    // Server control and answering are refused.
    let out = s.in_pane(&a, "$VIBEKE_BIN server stop 2>&1 | head -c 200");
    assert!(out.contains("permission_denied"), "{out}");
    let out = s.in_pane(&a, "$VIBEKE_BIN ask answer i1 --allow 2>&1 | head -c 200");
    assert!(
        out.contains("permission_denied") || out.contains("self_answer_forbidden"),
        "{out}"
    );
    // A pane may split itself and drive the child it created.
    let out = s.in_pane(&a, "C=$($VIBEKE_BIN pane split --current --direction down | sed -E 's/.*\"id\":\"([^\"]+)\".*/\\1/' | head -c 40); $VIBEKE_BIN pane send-text $C 'echo child-ok' --json >/dev/null && echo split-and-send-ok");
    assert!(
        out.contains("split-and-send-ok"),
        "own child pane must be drivable: {out}"
    );
    // Reads stay open.
    let out = s.in_pane(&a, "$VIBEKE_BIN pane list --json | head -c 20");
    assert!(out.contains("panes"), "{out}");
    // The user's CLI (outside panes) is unrestricted.
    s.json(&["pane", "send-text", &b, "echo from-user\n"]);
}
