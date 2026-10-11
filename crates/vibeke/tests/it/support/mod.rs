//! Shared harness for the integration tests (chaos gaps, timings, plugins): an isolated session
//! with its own runtime/state/config dirs, driven through the real `vibeke` binary. Dropping it
//! stops the server and kills its panes.
//!
//! [`Session::builder`] adds HOME and environment control (spec 18 §5, §12): a config text,
//! extra variables, a fresh HOME with XDG dirs and TMPDIR, removed variables, and an
//! allowlist that replaces the inherited environment.
#![allow(dead_code)]

pub mod plugins;
pub mod real_plugins;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Variables that would point a test command at the user's own Vibeke or Herdr.
const ALWAYS_REMOVED: &[&str] = &[
    "VIBEKE",
    "VIBEKE_SOCKET",
    "VIBEKE_SESSION",
    "VIBEKE_PANE_TOKEN",
    "VIBEKE_HERDR_BROKER",
    "HERDR_ENV",
    "HERDR_SOCKET_PATH",
    "HERDR_PANE_ID",
    "HERDR_BIN_PATH",
    "HERDR_SESSION",
    "HERDR_PLUGIN_ID",
];

pub struct Session {
    pub dir: tempfile::TempDir,
    /// Extra variables, applied after the session's own (so they win).
    pub env: Vec<(String, String)>,
    /// Inherited variables to remove.
    pub remove: Vec<String>,
    /// A fresh HOME: HOME, XDG_* and TMPDIR all point inside it.
    pub home: Option<PathBuf>,
    /// `Some(names)`: start from an empty environment and copy only these inherited names.
    pub allow: Option<Vec<String>>,
}

/// Builds a [`Session`]. `Session::new()` is `Session::builder().build()`.
#[derive(Default)]
pub struct SessionBuilder {
    prefix: Option<String>,
    config: Option<String>,
    env: Vec<(String, String)>,
    remove: Vec<String>,
    home: Option<PathBuf>,
    allow: Option<Vec<String>>,
}

impl SessionBuilder {
    /// The temp dir prefix under `/tmp` (keep it short: socket paths are length-limited).
    pub fn prefix(mut self, p: &str) -> Self {
        self.prefix = Some(p.to_string());
        self
    }
    /// Text of the session's `config.toml`.
    pub fn config(mut self, text: &str) -> Self {
        self.config = Some(text.to_string());
        self
    }
    pub fn env(mut self, k: &str, v: impl AsRef<std::ffi::OsStr>) -> Self {
        self.env
            .push((k.to_string(), v.as_ref().to_string_lossy().into_owned()));
        self
    }
    pub fn envs(mut self, kv: &[(&str, &str)]) -> Self {
        for (k, v) in kv {
            self.env.push((k.to_string(), v.to_string()));
        }
        self
    }
    pub fn remove(mut self, k: &str) -> Self {
        self.remove.push(k.to_string());
        self
    }
    /// HOME, XDG_CONFIG_HOME, XDG_DATA_HOME, XDG_STATE_HOME, XDG_CACHE_HOME, XDG_RUNTIME_DIR
    /// and TMPDIR all live under `dir` (created by [`SessionBuilder::build`]).
    pub fn home(mut self, dir: &Path) -> Self {
        self.home = Some(dir.to_path_buf());
        self
    }
    /// Drop the inherited environment except these names.
    pub fn allow_only(mut self, names: &[String]) -> Self {
        self.allow = Some(names.to_vec());
        self
    }
    pub fn build(self) -> Session {
        let dir = tempfile::Builder::new()
            // Short prefix under /tmp: unix socket paths are length-limited.
            .prefix(self.prefix.as_deref().unwrap_or("vk1d"))
            .tempdir_in("/tmp")
            .unwrap();
        if let Some(text) = &self.config {
            std::fs::write(dir.path().join("config.toml"), text).unwrap();
        }
        if let Some(h) = &self.home {
            for (_, d) in home_vars(h) {
                std::fs::create_dir_all(d).unwrap();
            }
        }
        Session {
            dir,
            env: self.env,
            remove: self.remove,
            home: self.home,
            allow: self.allow,
        }
    }
}

/// The variables a fresh HOME sets, with their directories.
fn home_vars(h: &Path) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("HOME", h.to_path_buf()),
        ("XDG_CONFIG_HOME", h.join(".config")),
        ("XDG_DATA_HOME", h.join(".local/share")),
        ("XDG_STATE_HOME", h.join(".local/state")),
        ("XDG_CACHE_HOME", h.join(".cache")),
        ("XDG_RUNTIME_DIR", h.join(".run")),
        ("TMPDIR", h.join("tmp")),
    ]
}

/// The `vibeke` binary under test. Nextest archives remap paths, so its variable wins.
pub fn bin() -> PathBuf {
    std::env::var_os("NEXTEST_BIN_EXE_vibeke")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_vibeke")))
}

impl Session {
    pub fn new() -> Self {
        Self::builder().build()
    }

    pub fn builder() -> SessionBuilder {
        SessionBuilder::default()
    }

    /// A path inside the session dir (canonical, so it matches what the server reports).
    pub fn path(&self, p: &str) -> PathBuf {
        self.dir.path().canonicalize().unwrap().join(p)
    }

    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(bin());
        if let Some(names) = &self.allow {
            c.env_clear();
            for n in names {
                if let Some(v) = std::env::var_os(n) {
                    c.env(n, v);
                }
            }
        }
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            // Never the user's gateway (the server supervises the one set up there).
            .env("VIBEKE_GATEWAY_DIR", d.join("gateway"));
        for k in ALWAYS_REMOVED
            .iter()
            .copied()
            .chain(self.remove.iter().map(String::as_str))
        {
            c.env_remove(k);
        }
        if let Some(h) = &self.home {
            for (k, v) in home_vars(h) {
                c.env(k, v);
            }
        }
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c.arg("--json").args(args);
        c
    }

    pub fn json(&self, args: &[&str]) -> Value {
        let out = self.cmd(args).output().expect("run vibeke");
        assert!(
            out.status.success(),
            "vibeke {args:?}: {}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
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
