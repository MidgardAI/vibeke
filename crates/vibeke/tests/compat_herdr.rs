//! Herdr compatibility layer end to end against isolated Vibeke sessions (07 §7.7, §8; M5
//! first slice): the `herdr` CLI shim, the compat listener's wire protocol and event stream,
//! and Herdr plugins through install → trust → action → broker callback → event hook →
//! revocation, including the no-escalation rule for pane-scoped callers.
//!
//! Every test uses temp VIBEKE_* dirs; nothing touches a real Herdr, its config or sockets.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new(config: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkm5")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        Session { dir }
    }
    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().canonicalize().unwrap().join(p)
    }
    /// The path as the CLI/server see it (env values are not canonicalized).
    fn env_path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env("XDG_DATA_HOME", d.join("data"))
            .env(
                "VIBEKE_NOTIFIER",
                format!("log:{}", d.join("notes.jsonl").display()),
            );
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
            "VIBEKE_HERDR_BROKER",
            "HERDR_ENV",
            "HERDR_SOCKET_PATH",
            "HERDR_PANE_ID",
            "HERDR_BIN_PATH",
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
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn herdr(&self, args: &[&str]) -> Value {
        let mut a = vec!["compat", "herdr"];
        a.extend(args);
        self.json(&a)
    }
    /// `(exit code, stderr json)` for a call expected to fail.
    fn fail(&self, args: &[&str]) -> (i32, Value) {
        let out = self.cmd(args).output().unwrap();
        assert!(
            !out.status.success(),
            "{args:?} should fail: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        (
            out.status.code().unwrap_or(-1),
            serde_json::from_slice(&out.stderr).unwrap_or(Value::Null),
        )
    }
    fn compat_socket(&self) -> PathBuf {
        self.path("run/herdr-compat/herdr.sock")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn wait_for(what: &str, timeout_ms: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

/// One raw Herdr request on a fresh connection; returns the response and whether the server
/// closed the connection afterwards.
fn raw(sock: &Path, line: &str) -> (Value, bool) {
    let mut st = UnixStream::connect(sock).unwrap();
    st.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    st.write_all(line.as_bytes()).unwrap();
    st.write_all(b"\n").unwrap();
    let mut rd = BufReader::new(st);
    let mut resp = String::new();
    rd.read_line(&mut resp).unwrap();
    let mut more = String::new();
    let closed = rd.read_line(&mut more).map(|n| n == 0).unwrap_or(false);
    (serde_json::from_str(&resp).unwrap(), closed)
}

/// No Vibeke ULID (26 Crockford base32 chars) may leak into compat responses.
fn assert_no_ulids(v: &Value) {
    let text = v.to_string();
    let re = regex_lite(&text);
    assert!(!re, "ULID leaked into compat output: {text}");
}

fn regex_lite(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        w.len() == 26
            && w.starts_with("01")
            && w.chars()
                .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
    })
}

#[test]
fn herdr_cli_shim_and_wire_protocol() {
    let s_ = Session::new("[compat.herdr]\nenabled = true\n");
    let v = s_.herdr(&["--version"]);
    // `--version` prints text, not JSON.
    assert!(v.is_null());
    let out = s_.cmd(&["compat", "herdr", "--version"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("herdr 0.9.3"));

    let created = s_.herdr(&["workspace", "create", "--cwd", "/tmp", "--label", "hw"]);
    assert_eq!(created["type"], "workspace_created");
    let ws = s(&created["workspace"]["workspace_id"]);
    let pane = s(&created["root_pane"]["pane_id"]);
    assert!(ws.starts_with('w'), "{created}");
    assert!(pane.starts_with(&format!("{ws}:p")), "{created}");
    assert_eq!(created["workspace"]["label"], "hw");
    assert_eq!(created["tab"]["workspace_id"], ws.as_str());
    assert_no_ulids(&created);

    let list = s_.herdr(&["workspace", "list"]);
    assert_eq!(list["type"], "workspace_list");
    assert!(
        list["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["workspace_id"] == ws.as_str() && w["number"] == 1 && w["pane_count"] == 1)
    );

    let panes = s_.herdr(&["pane", "list"]);
    assert_eq!(panes["type"], "pane_list");
    let p = panes["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["pane_id"] == pane.as_str())
        .cloned()
        .unwrap();
    assert_eq!(p["workspace_id"], ws.as_str());
    assert!(p.get("revision").is_some() && p.get("agent_status").is_some());
    assert_no_ulids(&panes);

    // Input and output through the shim.
    s_.herdr(&["pane", "run", &pane, "echo compat-$((40+2))"]);
    let m = s_.herdr(&[
        "pane",
        "wait-output",
        &pane,
        "--match",
        "compat-42",
        "--timeout-ms",
        "15000",
    ]);
    assert_eq!(m["type"], "pane_output_matched");
    let r = s_.herdr(&["pane", "read", &pane, "--source", "recent", "--lines", "30"]);
    assert_eq!(r["type"], "pane_read");
    assert!(s(&r["read"]["text"]).contains("compat-42"), "{r}");

    let renamed = s_.herdr(&["pane", "rename", &pane, "logs"]);
    assert_eq!(renamed["pane"]["label"], "logs");
    let tab = s_.herdr(&["tab", "create", "--workspace-id", &ws, "--label", "second"]);
    assert_eq!(tab["type"], "tab_created");
    assert_eq!(tab["tab"]["label"], "second");

    // Errors keep Herdr's shape and exit 1; known-but-missing methods are explicit.
    let (code, e) = s_.fail(&["compat", "herdr", "pane", "get", "w99:p99"]);
    assert_eq!(
        (code, s(&e["error"]["code"])),
        (1, "pane_not_found".to_string()),
        "{e}"
    );
    let (_, e) = s_.fail(&["compat", "herdr", "server", "stop"]);
    assert_eq!(e["error"]["code"], "unsupported", "{e}");
    let (code, e) = s_.fail(&["compat", "herdr", "popup", "close"]);
    assert_eq!(
        (code, s(&e["error"]["code"])),
        (1, "popup_not_found".to_string()),
        "no popup open: {e}"
    );
    let (code, _) = s_.fail(&["compat", "herdr", "pane", "teleport"]);
    assert_eq!(code, 2, "usage error");
    let (_, e) = s_.fail(&["compat", "herdr", "integration", "install", "claude"]);
    assert_eq!(e["error"]["kind"], "unsupported");

    // The public listener: Herdr wire format, one request per connection.
    let sock = s_.compat_socket();
    wait_for("compat socket", 5000, || sock.exists());
    let (v, closed) = raw(&sock, r#"{"id":"7","method":"workspace.list","params":{}}"#);
    assert_eq!(v["id"], "7");
    assert_eq!(v["result"]["type"], "workspace_list");
    assert!(v.get("jsonrpc").is_none());
    assert!(closed, "connection closes after the response");
    let (v, _) = raw(&sock, r#"{"id":1,"method":"workspace.list"}"#);
    assert_eq!(v["id"], "");
    assert_eq!(v["error"]["code"], "invalid_request");
    let (v, _) = raw(&sock, r#"{"id":"x","method":"galaxy.explode","params":{}}"#);
    assert_eq!(v["error"]["code"], "method_not_found");
    let (v, _) = raw(&sock, r#"{"id":"y","method":"server.stop","params":{}}"#);
    assert_eq!(v["error"]["code"], "unsupported");
    let (v, _) = raw(&sock, "{oops");
    assert_eq!(v["error"]["code"], "parse_error");
    let (v, _) = raw(&sock, r#"{"id":"z","method":"ping"}"#);
    assert_eq!(v["result"]["version"], "0.9.3");

    // Event stream: subscribe, then create a tab from another client.
    let mut st = UnixStream::connect(&sock).unwrap();
    st.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    st.write_all(br#"{"id":"s","method":"events.subscribe","params":{"subscriptions":[{"type":"tab.created"},{"type":"pane.created"}]}}"#).unwrap();
    st.write_all(b"\n").unwrap();
    let mut rd = BufReader::new(st);
    let mut ack = String::new();
    rd.read_line(&mut ack).unwrap();
    let ack: Value = serde_json::from_str(&ack).unwrap();
    assert_eq!(ack["result"]["type"], "subscription_started", "{ack}");
    let (v, _) = raw(
        &sock,
        &format!(
            r#"{{"id":"t","method":"tab.create","params":{{"workspace_id":"{ws}","label":"third"}}}}"#
        ),
    );
    assert_eq!(v["result"]["type"], "tab_created", "{v}");
    let mut seen = Vec::new();
    while seen.len() < 2 {
        let mut l = String::new();
        rd.read_line(&mut l).unwrap();
        let ev: Value = serde_json::from_str(&l).unwrap();
        assert_no_ulids(&ev);
        seen.push(s(&ev["event"]));
        if ev["event"] == "tab_created" {
            assert_eq!(ev["data"]["workspace_id"], ws.as_str());
            assert_eq!(ev["data"]["tab"]["label"], "third");
        }
    }
    assert!(seen.contains(&"tab_created".to_string()), "{seen:?}");
    assert!(seen.contains(&"pane_created".to_string()), "{seen:?}");

    // Pane-scoped subscription rule.
    let (v, _) = raw(
        &sock,
        r#"{"id":"q","method":"events.subscribe","params":{"subscriptions":[{"type":"pane.agent_status_changed"}]}}"#,
    );
    assert_eq!(v["error"]["code"], "invalid_params");

    let st = s_.json(&["compat", "status"]);
    assert_eq!(st["support"], "partial");
    assert_eq!(st["listener"]["live"], true);
    assert!(st["inventory"]["missing"].as_u64().unwrap() > 0);
}

#[test]
fn compat_listener_is_off_by_default_and_shim_installs_only_into_vibeke_dirs() {
    let s_ = Session::new("");
    let created = s_.herdr(&["workspace", "create", "--cwd", "/tmp"]);
    assert_eq!(
        created["type"], "workspace_created",
        "shim works without the listener"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(!s_.compat_socket().exists(), "listener must be opt-in");

    let r = s_.json(&["compat", "install-shim"]);
    let link = PathBuf::from(s(&r["shim"]));
    assert!(link.starts_with(s_.env_path("data")), "{r}");
    assert_eq!(link.file_name().unwrap(), "herdr");
    // The installed `herdr` is the shim: it reports the emulated baseline.
    let out = Command::new(&link)
        .arg("--version")
        .env("VIBEKE_RUNTIME_DIR", s_.path("run"))
        .env("VIBEKE_STATE_DIR", s_.path("state"))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("herdr 0.9.3"));
    let (code, _) = s_.fail(&["compat", "install-shim", "--dir", "/tmp/not-vibeke-bin"]);
    assert_eq!(code, 5);
    let r = s_.json(&["compat", "uninstall-shim"]);
    assert_eq!(r["removed"], true);
}

fn write_plugin(dir: &Path) {
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(
        dir.join("herdr-plugin.toml"),
        r#"id = "acme.probe"
name = "Probe"
version = "0.1.0"
min_herdr_version = "0.9.0"
platforms = ["linux", "macos"]

[[build]]
command = ["sh", "-c", "env | grep -c '^HERDR_SOCKET_PATH=' > built.txt; true"]

[[actions]]
id = "probe"
title = "Probe the session"
contexts = ["pane", "workspace"]
command = ["sh", "bin/probe.sh"]

[[actions]]
id = "slow"
title = "Slow callback"
command = ["sh", "bin/slow.sh"]

[[events]]
on = "tab.created"
command = ["sh", "bin/hook.sh"]
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("bin/probe.sh"),
        "herdr pane list > \"$HERDR_PLUGIN_STATE_DIR/panes.json\"\n\
         env | grep '^HERDR_' | sort > \"$HERDR_PLUGIN_STATE_DIR/env.txt\"\n\
         echo done\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("bin/slow.sh"),
        "sleep 2\n\
         \"$HERDR_BIN_PATH\" workspace list > \"$HERDR_PLUGIN_STATE_DIR/slow.out\" 2> \"$HERDR_PLUGIN_STATE_DIR/slow.err\"\n\
         echo $? > \"$HERDR_PLUGIN_STATE_DIR/slow.code\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("bin/hook.sh"),
        "echo \"$HERDR_PLUGIN_EVENT $HERDR_TAB_ID\" >> \"$HERDR_PLUGIN_STATE_DIR/hooks.txt\"\n",
    )
    .unwrap();
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

#[test]
fn herdr_plugin_install_trust_actions_hooks_and_revocation() {
    let s_ = Session::new("");
    let src = s_.path("src/probe");
    write_plugin(&src);
    let state = s_.path("state/plugins/state/acme.probe");

    // Install: copied into a managed checkout, inactive, nothing built.
    let r = s_.json(&["plugin", "install", src.to_str().unwrap()]);
    assert_eq!(r["plugin"]["status"], "untrusted", "{r}");
    let root = PathBuf::from(s(&r["plugin"]["root"]));
    assert!(
        root.starts_with(s_.env_path("state/plugins/checkouts")),
        "{}",
        root.display()
    );
    assert!(!root.join("built.txt").exists(), "no build before trust");
    assert!(s(&r["trust_terms"]).contains("probe [pane,workspace]"));
    assert!(s(&r["trust_terms"]).contains("tab.created: sh bin/hook.sh"));

    // Nothing runs before trust.
    let ws = s_.herdr(&["workspace", "create", "--cwd", "/tmp"]);
    let pane = s(&ws["root_pane"]["pane_id"]);
    let wsid = s(&ws["workspace"]["workspace_id"]);
    let (code, e) = s_.fail(&["plugin", "action", "run", "acme.probe", "probe"]);
    assert_eq!(code, 5, "{e}");
    assert!(s(&e["error"]["message"]).contains("untrusted"), "{e}");
    let acts = s_.json(&["plugin", "action", "list"]);
    assert_eq!(acts["actions"][0]["available"], false);

    // Trust needs --legacy explicitly; then the build runs without socket authority.
    let (code, _) = s_.fail(&["plugin", "trust", "acme.probe"]);
    assert_eq!(code, 5);
    let r = s_.json(&["plugin", "trust", "acme.probe", "--legacy"]);
    assert_eq!(r["plugin"]["status"], "active", "{r}");
    assert_eq!(r["grant"]["mode"], "herdr_legacy");
    assert_eq!(
        read(&root.join("built.txt")).trim(),
        "0",
        "build env has no HERDR_SOCKET_PATH"
    );

    // Action: async log record, broker callback via bare `herdr` on the private PATH.
    let r = s_.json(&[
        "plugin",
        "action",
        "run",
        "acme.probe",
        "probe",
        "--pane",
        &pane,
    ]);
    let log_id = s(&r["log"]["log_id"]);
    assert!(
        matches!(r["log"]["status"].as_str(), Some("running" | "succeeded")),
        "{r}"
    );
    let mut last = Value::Null;
    wait_for("probe to complete", 15000, || {
        last = s_.json(&["plugin", "logs", "--plugin", "acme.probe"]);
        last["logs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["log_id"] == log_id.as_str() && l["status"] != "running")
    });
    let log = last["logs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["log_id"] == log_id.as_str())
        .unwrap()
        .clone();
    assert_eq!(log["status"], "succeeded", "{log}");
    assert_eq!(log["exit_code"], 0);
    assert!(s(&log["stdout"]).contains("done"));
    let panes: Value =
        serde_json::from_str(&read(&state.join("panes.json"))).unwrap_or(Value::Null);
    assert_eq!(
        panes["type"], "pane_list",
        "callback result: {panes} / {log}"
    );
    assert!(
        panes["panes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["pane_id"] == pane.as_str())
    );
    let env = read(&state.join("env.txt"));
    assert!(env.contains("HERDR_ENV=1"), "{env}");
    assert!(env.contains("HERDR_PLUGIN_ID=acme.probe"));
    assert!(env.contains(&format!("HERDR_PANE_ID={pane}")), "{env}");
    assert!(env.contains(&format!("HERDR_WORKSPACE_ID={wsid}")), "{env}");
    assert!(env.contains("HERDR_PLUGIN_ACTION_ID=probe"));
    let sock_line = env
        .lines()
        .find(|l| l.starts_with("HERDR_SOCKET_PATH="))
        .unwrap();
    assert!(sock_line.contains("/herdr-compat/brokers/"), "{sock_line}");
    assert!(!sock_line.contains(".config/herdr"));
    assert!(
        env.lines()
            .any(|l| l.starts_with("HERDR_BIN_PATH=") && l.ends_with("/herdr-compat/bin/herdr"))
    );

    // `[[events]]` hook on a projected Herdr event.
    s_.json(&["tab", "create", "--workspace", &wsid]);
    wait_for("tab.created hook", 15000, || {
        read(&state.join("hooks.txt")).contains("tab.created")
    });
    assert!(read(&state.join("hooks.txt")).contains(&format!("tab.created {wsid}:t")));

    // No escalation: an agent in a pane cannot run the legacy plugin through the API.
    let bin = env!("CARGO_BIN_EXE_vibeke");
    let marker = s_.path("escalate.out");
    s_.json(&[
        "pane",
        "run",
        &pane,
        &format!(
            "{bin} --json plugin action run acme.probe probe 2> {}; echo rc=$? >> {}",
            marker.display(),
            marker.display()
        ),
    ]);
    wait_for("escalation attempt", 15000, || {
        read(&marker).contains("rc=")
    });
    let out = read(&marker);
    assert!(
        out.contains("permission_denied") && out.contains("rc=5"),
        "{out}"
    );

    // Revocation reaches a running invocation's broker: the slow action's callback fails.
    s_.json(&["plugin", "action", "run", "acme.probe.slow"]);
    s_.json(&["plugin", "untrust", "acme.probe"]);
    wait_for("slow action", 15000, || state.join("slow.code").exists());
    assert_ne!(read(&state.join("slow.code")).trim(), "0");
    assert!(
        read(&state.join("slow.err")).contains("permission_denied"),
        "{}",
        read(&state.join("slow.err"))
    );
    let (code, _) = s_.fail(&["plugin", "action", "run", "acme.probe", "probe"]);
    assert_eq!(code, 5);

    // A changed manifest invalidates a re-granted trust.
    s_.json(&["plugin", "trust", "acme.probe", "--legacy"]);
    let mf = root.join("herdr-plugin.toml");
    let text = read(&mf);
    std::fs::write(&mf, text + "\n# edited\n").unwrap();
    let l = s_.json(&["plugin", "list"]);
    assert_eq!(l["plugins"][0]["status"], "stale_trust");

    // Disable, unlink-vs-uninstall rules, config dir.
    let cd = s_.json(&["plugin", "config-dir", "acme.probe"]);
    assert!(s(&cd["config_dir"]).ends_with("plugins/acme.probe"));
    let (code, _) = s_.fail(&["plugin", "unlink", "acme.probe"]);
    assert_eq!(code, 1, "installed plugins are uninstalled, not unlinked");
    s_.json(&["plugin", "uninstall", "acme.probe"]);
    assert!(!root.exists());
    assert!(src.join("herdr-plugin.toml").exists(), "source untouched");
    assert!(state.join("env.txt").exists(), "plugin state preserved");
}

#[test]
fn herdr_shim_plugin_commands_and_link() {
    let s_ = Session::new("");
    let src = s_.path("src/probe");
    write_plugin(&src);
    // Herdr's CLI grammar for the registry.
    let r = s_.herdr(&["plugin", "link", src.to_str().unwrap()]);
    assert_eq!(r["plugin"]["status"], "untrusted");
    assert_eq!(r["plugin"]["managed"], false);
    assert!(!src.join("built.txt").exists(), "link never builds");
    let l = s_.herdr(&["plugin", "list"]);
    assert_eq!(l["plugins"][0]["plugin_id"], "acme.probe");
    s_.herdr(&["plugin", "disable", "acme.probe"]);
    s_.json(&["plugin", "trust", "acme.probe", "--legacy"]);
    let l = s_.herdr(&["plugin", "list"]);
    assert_eq!(l["plugins"][0]["status"], "disabled");
    s_.herdr(&["plugin", "enable", "acme.probe"]);
    let acts = s_.herdr(&["plugin", "action", "list"]);
    assert_eq!(acts["type"], "plugin_action_list");
    assert!(
        acts["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["qualified_id"] == "acme.probe.probe" && a["available"] == true)
    );
    let ws = s_.herdr(&["workspace", "create", "--cwd", "/tmp"]);
    let _ = ws;
    let inv = s_.herdr(&["plugin", "action", "invoke", "acme.probe.probe"]);
    assert_eq!(inv["type"], "plugin_action_started", "{inv}");
    let logs = s_.herdr(&["plugin", "log", "list", "acme.probe"]);
    assert_eq!(logs["type"], "plugin_log_list");
    s_.herdr(&["plugin", "unlink", "acme.probe"]);
    assert!(src.join("herdr-plugin.toml").exists(), "unlink keeps files");
    let l = s_.herdr(&["plugin", "list"]);
    assert!(l["plugins"].as_array().unwrap().is_empty());
}

/// Stop the server and wait until it no longer answers.
fn stop(s_: &Session) {
    let _ = s_.cmd(&["server", "stop"]).output();
    wait_for("server to stop", 10000, || {
        s_.cmd(&["--no-spawn", "server", "status"])
            .output()
            .is_ok_and(|o| !o.status.success())
    });
}

#[test]
fn startup_hooks_run_once_per_server_activation_and_only_when_trusted() {
    let s_ = Session::new("");
    let src = s_.path("src/boot");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("herdr-plugin.toml"),
        "id = \"acme.boot\"\n[[startup]]\ncommand = [\"sh\", \"-c\", \"echo up >> \\\"$HERDR_PLUGIN_STATE_DIR/boots.txt\\\"\"]\n",
    )
    .unwrap();
    let boots = s_.path("state/plugins/state/acme.boot/boots.txt");
    let count = || read(&boots).lines().count();
    s_.json(&["plugin", "link", src.to_str().unwrap()]);
    s_.json(&["workspace", "list"]); // spawns the server: untrusted, so no startup
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(count(), 0, "untrusted startup hooks never run");
    stop(&s_);
    s_.json(&["plugin", "trust", "acme.boot", "--legacy"]);
    s_.json(&["workspace", "list"]);
    wait_for("startup hook", 10000, || count() == 1);
    // Attaching more clients or reloading does not rerun it.
    s_.json(&["workspace", "list"]);
    s_.json(&["server", "reload-config"]);
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(count(), 1);
    stop(&s_);
    s_.json(&["workspace", "list"]);
    wait_for("second activation", 10000, || count() == 2);
}
