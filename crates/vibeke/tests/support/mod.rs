//! Shared harness for the 1D tests (chaos gaps, timings): an isolated session with its own
//! runtime/state/config dirs, driven through the real `vibeke` binary. Dropping it stops the
//! server and kills its panes.
#![allow(dead_code)]

use serde_json::Value;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct Session {
    pub dir: tempfile::TempDir,
}

impl Session {
    pub fn new() -> Self {
        Session {
            // Short prefix under /tmp: unix socket paths are length-limited.
            dir: tempfile::Builder::new()
                .prefix("vk1d")
                .tempdir_in("/tmp")
                .unwrap(),
        }
    }

    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            // Never the user's gateway (the server supervises the one set up there).
            .env("VIBEKE_GATEWAY_DIR", d.join("gateway"));
        c.env_remove("VIBEKE")
            .env_remove("VIBEKE_SOCKET")
            .env_remove("VIBEKE_SESSION")
            .env_remove("VIBEKE_PANE_TOKEN");
        c.arg("--json").args(args);
        c
    }

    pub fn json(&self, args: &[&str]) -> Value {
        let out = self.cmd(args).output().expect("run vibeke");
        assert!(
            out.status.success(),
            "vibeke {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }

    pub fn server_pid(&self) -> i32 {
        std::fs::read_to_string(self.dir.path().join("run/default/server.pid"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    pub fn kill_server(&self) {
        let pid = self.server_pid();
        assert!(pid > 0, "no server pid");
        // SAFETY: killing our own server process.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(pid as i64) {
            assert!(Instant::now() < deadline, "server {pid} did not die");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn socket(&self) -> std::path::PathBuf {
        self.dir.path().join("run/default/vibeke.sock")
    }

    pub fn panes(&self) -> Vec<Value> {
        self.json(&["pane", "list"])["panes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    pub fn pane(&self, id: &str) -> Value {
        self.panes()
            .into_iter()
            .find(|p| p["id"] == id)
            .unwrap_or(Value::Null)
    }

    pub fn read(&self, pane: &str, source: &str, lines: &str) -> String {
        self.json(&["pane", "read", pane, "--source", source, "--lines", lines])["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    pub fn wait_output(&self, pane: &str, pat: &str, timeout_ms: u64) {
        let r = self
            .cmd(&[
                "pane",
                "wait-output",
                pane,
                pat,
                "--timeout-ms",
                &timeout_ms.to_string(),
            ])
            .output()
            .unwrap();
        assert!(r.status.success(), "{pat:?} not seen in {pane}");
    }

    /// A workspace whose root pane runs `command`; returns the root pane id.
    pub fn workspace(&self, command: &str) -> String {
        let ws = self.json(&["workspace", "create", "--cwd", "/tmp", "--command", command]);
        ws["root_pane"]["id"].as_str().unwrap().to_string()
    }

    pub fn split(&self, from: &str, command: &str) -> String {
        let p = self.json(&[
            "pane",
            "split",
            from,
            "--direction",
            "right",
            "--command",
            command,
        ]);
        p["pane"]["id"].as_str().unwrap().to_string()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

/// A raw JSON-RPC connection to the session socket.
pub struct Rpc {
    rd: std::io::BufReader<std::os::unix::net::UnixStream>,
    wr: std::os::unix::net::UnixStream,
    id: u64,
}

impl Rpc {
    pub fn connect(path: &std::path::Path) -> Rpc {
        let s = std::os::unix::net::UnixStream::connect(path).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        Rpc {
            rd: std::io::BufReader::new(s.try_clone().unwrap()),
            wr: s,
            id: 0,
        }
    }

    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        use std::io::{BufRead, Write};
        self.id += 1;
        let req = serde_json::json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params});
        writeln!(self.wr, "{req}").unwrap();
        loop {
            let mut line = String::new();
            assert!(
                self.rd.read_line(&mut line).unwrap() > 0,
                "closed in {method}"
            );
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == serde_json::json!(self.id) {
                return match v.get("error") {
                    Some(e) if !e.is_null() => Err(e.clone()),
                    _ => Ok(v["result"].clone()),
                };
            }
        }
    }
}

/// The holder is the parent of the pane's child process.
pub fn parent_pid(pid: i64) -> i32 {
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

pub fn alive(pid: i64) -> bool {
    // SAFETY: signal 0 only checks existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Timing bounds: generous by default (shared CI runners), the spec 10 §1.4 budgets when
/// `VIBEKE_PERF_STRICT` is set (quiet reference machine).
pub fn bound(strict_ms: u64, generous_ms: u64) -> Duration {
    Duration::from_millis(if std::env::var_os("VIBEKE_PERF_STRICT").is_some() {
        strict_ms
    } else {
        generous_ms
    })
}
