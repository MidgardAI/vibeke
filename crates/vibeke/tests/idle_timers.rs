//! Idle wakeup budget (spec 10 §1.3): idle panes arm no periodic timers, housekeeping and the
//! sandbox tick stay asleep, preview discovery backs off — and still finds a server as soon as
//! the pane prints something. Observed through `server.status` → `timers`.

use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkidle")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(dir.path().join("config.toml"), "").unwrap();
        Session { dir }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env("VIBEKE_TEST_HOOKS", "1")
            // Leave the fast discovery period 1 s after the last activity (default 30 s).
            .env("VIBEKE_PREVIEW_FAST_WINDOW_MS", "1000");
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
            "VIBEKE_PANE_ID",
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
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            self.log_tail()
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(self.path().join("state/default/logs/server.log"))
            .unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        lines[lines.len().saturating_sub(30)..].join("\n")
    }
    fn timers(&self) -> Value {
        self.json(&["api", "call", "server.status", "{}"])["timers"].clone()
    }
    fn wait(&self, what: &str, secs: u64, f: impl Fn(&Value) -> bool) -> Value {
        let t0 = Instant::now();
        loop {
            let t = self.timers();
            if f(&t) {
                return t;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(secs),
                "timed out waiting for {what}: {t}\n{}",
                self.log_tail()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    fn previews(&self) -> Vec<Value> {
        self.json(&["preview", "list", "--all"])["previews"]
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

fn n(t: &Value, k: &str) -> u64 {
    t[k].as_u64()
        .unwrap_or_else(|| panic!("timers.{k} missing: {t}"))
}

#[test]
fn idle_panes_schedule_no_periodic_wakeups() {
    let s = Session::new();
    let mut panes = Vec::new();
    for cmd in ["/bin/cat", "/bin/sh", "/bin/cat"] {
        let p = serde_json::json!({"cwd": "/tmp", "command": [cmd]}).to_string();
        let v = s.json(&["api", "call", "workspace.create", &p]);
        panes.push(v["root_pane"]["id"].as_str().unwrap().to_string());
    }
    // Output in a pane arms its snapshot deadline; it fires once the pane is quiet and
    // nothing stays armed.
    s.json(&["pane", "run", &panes[1], "echo hello-idle"]);
    s.json(&[
        "pane",
        "wait-output",
        &panes[1],
        "--regex",
        "hello-idle",
        "--timeout-ms",
        "10000",
    ]);
    let t = s.wait("snapshot deadline fired, nothing pending", 15, |t| {
        n(t, "snapshot_fires") >= 1 && n(t, "snapshot_pending") == 0
    });
    assert!(n(&t, "snapshot_arms") >= 1, "{t}");
    // Discovery backs off once the panes are quiet.
    let t0 = s.wait("discovery back-off", 20, |t| {
        n(t, "discovery_interval_ms") >= 4000
    });
    assert_eq!(t0["sandbox_ticking"], false, "{t0}");
    std::thread::sleep(Duration::from_secs(4));
    let t1 = s.timers();
    // Fully idle: no deadline armed or fired, no housekeeping pass, no scheduler wakeup.
    for k in [
        "snapshot_arms",
        "snapshot_fires",
        "scheduler_wakeups",
        "housekeeping_runs",
    ] {
        assert_eq!(n(&t0, k), n(&t1, k), "{k} moved while idle: {t0} -> {t1}");
    }
    assert_eq!(n(&t1, "snapshot_pending"), 0, "{t1}");
    // At most one back-off discovery pass in 4 s (it was every 2 s before, per 30 panes).
    assert!(
        n(&t1, "discovery_passes") - n(&t0, "discovery_passes") <= 1,
        "{t0} -> {t1}"
    );
}

#[test]
fn discovery_finds_server_after_output_while_backed_off() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("python3 not found; skipping");
        return;
    }
    let s = Session::new();
    let pane = s.json(&["workspace", "create", "--cwd", "/tmp"])["root_pane"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    s.json(&[
        "pane",
        "wait-idle",
        &pane,
        "--quiet-ms",
        "600",
        "--timeout-ms",
        "10000",
    ]);
    // Prints at once (so its foreground change is reported then, not later), stays silent for
    // 7 s while discovery backs off to 8 s, then binds and announces — without a URL, so only
    // the listener scan can find it, and only the output wake-up finds it within 5 s (checked:
    // with the wake-up disabled this test fails).
    s.json(&[
        "pane",
        "run",
        &pane,
        "python3 -u -c \"import time, http.server as h, socketserver as ss; print('starting'); time.sleep(7); srv = ss.TCPServer(('127.0.0.1', 0), h.SimpleHTTPRequestHandler); print('bound port', srv.server_address[1]); srv.serve_forever()\"",
    ]);
    s.wait("discovery backed off", 15, |t| {
        n(t, "discovery_interval_ms") >= 4000
    });
    s.json(&[
        "pane",
        "wait-output",
        &pane,
        "--regex",
        r"bound port \d+",
        "--timeout-ms",
        "20000",
    ]);
    let seen = Instant::now();
    let text = s.json(&["pane", "read", &pane, "--source", "recent", "--lines", "20"])["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let i = text.rfind("bound port ").expect("port line") + "bound port ".len();
    let port: u64 = text[i..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap();
    loop {
        if let Some(p) = s.previews().into_iter().find(|p| p["port"] == port) {
            assert_eq!(p["source"], "listener", "{p}");
            assert_eq!(p["pane"], pane.as_str());
            break;
        }
        assert!(
            seen.elapsed() < Duration::from_secs(5),
            "not discovered within 5 s of the output (back-off fallback is up to 30 s): {}\n{}",
            s.timers(),
            s.log_tail()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Live preview: discovery is back on the fast period.
    s.wait("fast period with a live preview", 10, |t| {
        n(t, "discovery_interval_ms") == 2000
    });
}
