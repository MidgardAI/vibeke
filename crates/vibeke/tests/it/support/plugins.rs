//! Helpers for the plugin system tests (07 §7.6, §7.7): a session set up for plugins, API and
//! Herdr shim calls, plugin logs and events, and small file and Git helpers. Shared by
//! `compat_herdr_plugins_ext.rs`, `compat_herdr_real_plugins.rs` and
//! `compat_herdr_plugin_fixtures.rs` (spec 18 §12).

use super::Session;
use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// A session for plugin tests with `config` as its `config.toml`.
pub fn session(config: &str) -> Session {
    session_with_env(config, &[])
}

/// Like [`session`], with extra variables. A fake `GITHUB_TOKEN` lets tests check that plugins
/// never see the operator's environment; notifications go to a log file.
pub fn session_with_env(config: &str, env: &[(&str, &str)]) -> Session {
    let mut s = Session::builder()
        .prefix("vkpe")
        .config(config)
        .envs(env)
        .build();
    let d = s.dir.path().to_path_buf();
    let base = vec![
        (
            "XDG_DATA_HOME".to_string(),
            d.join("data").to_string_lossy().into_owned(),
        ),
        (
            "GITHUB_TOKEN".to_string(),
            "ghp_not_for_plugins".to_string(),
        ),
        (
            "VIBEKE_NOTIFIER".to_string(),
            format!("log:{}", d.join("notes.jsonl").display()),
        ),
    ];
    // The session's own variables come first, so the caller's `env` wins.
    s.env.splice(0..0, base);
    s
}

impl Session {
    /// The command's exit code and stderr; panics when it succeeds.
    pub fn fail(&self, args: &[&str]) -> (i32, String) {
        let out = self.cmd(args).output().unwrap();
        assert!(
            !out.status.success(),
            "{args:?} should fail: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    /// `vibeke compat herdr <args>`.
    pub fn herdr(&self, args: &[&str]) -> Value {
        let mut a = vec!["compat", "herdr"];
        a.extend(args);
        self.json(&a)
    }

    /// The stderr of a failing `vibeke compat herdr <args>`.
    pub fn herdr_fail_text(&self, args: &[&str]) -> String {
        let mut a = vec!["compat", "herdr"];
        a.extend(args);
        self.fail(&a).1
    }

    /// One native API call through `vibeke api call`.
    pub fn api(&self, method: &str, params: Value) -> Value {
        self.json(&["api", "call", method, &params.to_string()])
    }

    /// The `plugin.list` entry of `id`.
    pub fn plugin(&self, id: &str) -> Value {
        let l = self.api("plugin.list", json!({}));
        l["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["plugin_id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id} not listed: {l}"))
    }

    pub fn events(&self, types: &str) -> Vec<Value> {
        let v = self.api("events.read", json!({"types": types}));
        v["events"].as_array().cloned().unwrap_or_default()
    }

    /// Run an action and return the log record at launch.
    pub fn run(&self, plugin: &str, action: &str) -> Value {
        self.api(
            "plugin.action.run",
            json!({"plugin": plugin, "action": action}),
        )["log"]
            .clone()
    }

    /// Run an action that is refused before launch; returns the error text.
    pub fn run_fail(&self, plugin: &str, action: &str) -> String {
        self.fail(&[
            "api",
            "call",
            "plugin.action.run",
            &json!({"plugin": plugin, "action": action}).to_string(),
        ])
        .1
    }

    /// Wait until `plugin` has no running invocation; returns its records (oldest first).
    pub fn settle(&self, plugin: &str) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let l = self.api("plugin.log.list", json!({"plugin": plugin}));
            let logs = l["logs"].as_array().cloned().unwrap_or_default();
            if logs.iter().all(|x| x["status"] != "running") {
                return logs;
            }
            assert!(Instant::now() < deadline, "invocations never finished");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

pub fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

pub fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// Run `git` in `dir` with a fixed identity and no user or system config; panics on failure.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

/// Poll `f` every 100 ms until it holds; panics after `timeout_ms`.
pub fn wait_for(what: &str, timeout_ms: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

/// Poll `f` every 100 ms until it holds or `timeout` passes; returns whether it held.
pub fn poll(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
