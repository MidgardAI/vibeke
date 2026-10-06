//! Herdr compatibility: the 2026-10-06 M5 review findings, end to end against isolated Vibeke
//! sessions — trust bound to installed content, build-time manifest checks, concurrent registry
//! changes, broker lifetime and revocation, the hook cache, cross-session plugin identity and
//! pane session switching, the shim never reaching a non-Vibeke socket, and the baseline
//! request/response fixes (qualified action ids, log fields, `target_pane_id`, `layout.apply`
//! `root`, split-ratio `path`).
//!
//! Every test uses temp VIBEKE_* dirs. Nothing touches a real Herdr: the "live Herdr" below is a
//! plain Unix listener in the test's temp dir. Commands typed into panes set the isolated
//! VIBEKE_* dirs explicitly (pane environments do not carry them).

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new(config: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkm5r")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        Session { dir }
    }
    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().canonicalize().unwrap().join(p)
    }
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
            "HERDR_SESSION",
            "HERDR_PLUGIN_ID",
        ] {
            c.env_remove(k);
        }
        c.arg("--json").args(args);
        c
    }
    /// The isolated VIBEKE_* assignments as a shell prefix, for commands typed into panes.
    fn shell_env(&self) -> String {
        let d = self.dir.path();
        format!(
            "VIBEKE_RUNTIME_DIR={} VIBEKE_STATE_DIR={} VIBEKE_CONFIG={}",
            d.join("run").display(),
            d.join("state").display(),
            d.join("config.toml").display()
        )
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
    fn status(&self, id: &str) -> String {
        let l = self.json(&["plugin", "list"]);
        l["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["plugin_id"] == id)
            .map(|p| s(&p["status"]))
            .unwrap_or_else(|| "absent".into())
    }
    fn logs(&self, plugin: &str) -> Vec<Value> {
        self.json(&["plugin", "logs", "--plugin", plugin])["logs"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
    fn compat_socket(&self) -> PathBuf {
        self.path("run/herdr-compat/herdr.sock")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
        let _ = self
            .cmd(&["--session", "work", "server", "stop", "--kill-panes"])
            .output();
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
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

/// Write a plugin directory: manifest text plus `bin/<name>` scripts.
fn plugin(dir: &Path, manifest: &str, scripts: &[(&str, &str)]) {
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("herdr-plugin.toml"), manifest).unwrap();
    for (name, body) in scripts {
        std::fs::write(dir.join("bin").join(name), body).unwrap();
    }
}

/// One raw Herdr request on a fresh connection.
fn raw(sock: &Path, req: Value) -> Value {
    let mut st = UnixStream::connect(sock).unwrap();
    st.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    st.write_all(format!("{req}\n").as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(st).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap_or(Value::Null)
}

/// Read one line from a held connection: `None` when the server closed it.
fn read_line(st: &mut BufReader<UnixStream>) -> Option<Value> {
    let mut l = String::new();
    match st.read_line(&mut l) {
        Ok(0) | Err(_) => None,
        Ok(_) => serde_json::from_str(&l).ok(),
    }
}

fn wait_finished(s_: &Session, plugin: &str, log_id: &str) -> Value {
    let mut last = Value::Null;
    wait_for("invocation to finish", 20000, || {
        last = s_
            .logs(plugin)
            .into_iter()
            .find(|l| l["log_id"] == log_id)
            .unwrap_or(Value::Null);
        !last.is_null() && last["status"] != "running"
    });
    last
}

// ---- findings 1 and 8: content-bound trust, build-time manifest checks -------------------------

#[test]
fn reinstalled_content_needs_review_and_build_and_manifest_mutation_aborts_the_build() {
    let s_ = Session::new("");
    let src = s_.path("src/tool");
    let manifest = r#"id = "acme.tool"
[[build]]
command = ["sh", "-c", "echo b >> builds.txt"]
[[actions]]
id = "go"
title = "Go"
command = ["sh", "bin/go.sh"]
"#;
    plugin(
        &src,
        manifest,
        &[("go.sh", "echo v1 >> \"$HERDR_PLUGIN_STATE_DIR/ran.txt\"\n")],
    );
    let r = s_.json(&["plugin", "install", src.to_str().unwrap(), "--yes"]);
    assert_eq!(r["plugin"]["status"], "active", "{r}");
    let root = PathBuf::from(s(&r["plugin"]["root"]));
    assert_eq!(read(&root.join("builds.txt")).lines().count(), 1);

    // Same manifest bytes, different executable content: the reinstall is inactive.
    std::fs::write(
        src.join("bin/go.sh"),
        "echo v2 >> \"$HERDR_PLUGIN_STATE_DIR/ran.txt\"\n",
    )
    .unwrap();
    let r = s_.json(&["plugin", "install", src.to_str().unwrap()]);
    assert_eq!(r["plugin"]["status"], "untrusted", "{r}");
    assert!(r["plugin"]["trust"].is_null());
    let (code, e) = s_.fail(&["plugin", "action", "run", "acme.tool", "go"]);
    assert_eq!(code, 5, "{e}");
    assert!(
        !root.join("builds.txt").exists(),
        "fresh checkout, not built"
    );

    // Renewed review: the build runs again before anything executes.
    let r = s_.json(&["plugin", "trust", "acme.tool", "--legacy"]);
    assert_eq!(r["plugin"]["status"], "active", "{r}");
    assert_eq!(read(&root.join("builds.txt")).lines().count(), 1);
    s_.json(&["workspace", "create", "--cwd", "/tmp"]);
    let log = s_.json(&["plugin", "action", "run", "acme.tool.go"]);
    let done = wait_finished(&s_, "acme.tool", &s(&log["log"]["log_id"]));
    assert_eq!(done["status"], "succeeded", "{done}");
    assert_eq!(
        read(&s_.path("state/plugins/state/acme.tool/ran.txt")).trim(),
        "v2"
    );

    // A first build step that rewrites the manifest stops the build and the registration.
    let bad = s_.path("src/mutant");
    plugin(
        &bad,
        r#"id = "acme.mutant"
[[build]]
command = ["sh", "-c", "printf '\n[[startup]]\ncommand = [\"sh\", \"-c\", \"touch pwned\"]\n' >> herdr-plugin.toml"]
[[build]]
command = ["sh", "-c", "touch second-step-ran"]
"#,
        &[],
    );
    let (code, e) = s_.fail(&["plugin", "install", bad.to_str().unwrap(), "--yes"]);
    assert_eq!(code, 1, "{e}");
    assert_eq!(e["error"]["kind"], "build_failed", "{e}");
    assert!(
        s(&e["error"]["message"]).contains("manifest changed"),
        "{e}"
    );
    let checkout = s_.path("state/plugins/checkouts/acme.mutant");
    assert!(!checkout.join("second-step-ran").exists());
    assert_eq!(s_.status("acme.mutant"), "absent", "registration aborted");
}

// ---- finding 5: concurrent registry changes ----------------------------------------------------

#[test]
fn a_revocation_during_another_plugins_build_is_kept() {
    let s_ = Session::new("");
    let a = s_.path("src/a");
    plugin(
        &a,
        "id = \"acme.a\"\n[[actions]]\nid = \"go\"\ntitle = \"Go\"\ncommand = [\"sh\", \"bin/go.sh\"]\n",
        &[("go.sh", "true\n")],
    );
    s_.json(&["plugin", "link", a.to_str().unwrap(), "--yes"]);
    assert_eq!(s_.status("acme.a"), "active");
    let gate = s_.path("gate");
    std::fs::create_dir_all(&gate).unwrap();
    let b = s_.path("src/b");
    plugin(
        &b,
        &format!(
            "id = \"acme.b\"\n[[build]]\ncommand = [\"sh\", \"-c\", \"touch {g}/started; while [ ! -f {g}/go ]; do sleep 0.1; done\"]\n",
            g = gate.display()
        ),
        &[],
    );
    s_.json(&["plugin", "install", b.to_str().unwrap()]);
    let mut slow = s_
        .cmd(&["plugin", "trust", "acme.b", "--legacy"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for("B's build to start", 15000, || {
        gate.join("started").exists()
    });
    s_.json(&["plugin", "untrust", "acme.a"]);
    std::fs::write(gate.join("go"), "").unwrap();
    assert!(slow.wait().unwrap().success());
    assert_eq!(
        s_.status("acme.a"),
        "untrusted",
        "revocation survives B's save"
    );
    assert_eq!(s_.status("acme.b"), "active");
}

// ---- finding 6: the shim and non-Vibeke sockets ------------------------------------------------

#[test]
fn the_shim_never_talks_to_a_socket_that_is_not_a_registered_broker() {
    let s_ = Session::new("");
    // A fake "live Herdr": records anything that connects.
    let fake_dir = s_.path("fake-herdr");
    std::fs::create_dir_all(&fake_dir).unwrap();
    let fake = fake_dir.join("herdr.sock");
    let listener = UnixListener::bind(&fake).unwrap();
    listener.set_nonblocking(true).unwrap();
    let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (g2, st2) = (got.clone(), stop.clone());
    let t = std::thread::spawn(move || {
        while !st2.load(std::sync::atomic::Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut c, _)) => {
                    c.set_read_timeout(Some(Duration::from_millis(500))).ok();
                    let mut b = Vec::new();
                    let _ = c.read_to_end(&mut b);
                    g2.lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&b).into_owned());
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    });
    let brokers = s_.env_path("run/default/herdr-compat/brokers");
    std::fs::create_dir_all(&brokers).unwrap();
    // Traversal that lexically starts with the runtime root.
    let traversal = brokers.join("../../../../fake-herdr/herdr.sock");
    // A symlink inside the broker dir, even when listed in the broker registry.
    let link = brokers.join("evil.sock");
    std::os::unix::fs::symlink(&fake, &link).unwrap();
    std::fs::write(
        s_.env_path("run/default/herdr-compat/brokers.json"),
        json!([{"path": link, "plugin_id": "acme.x"}]).to_string(),
    )
    .unwrap();
    for sock in [&traversal, &link, &fake] {
        for args in [
            &["compat", "herdr", "workspace", "list"][..],
            &["compat", "herdr", "server", "stop"][..],
        ] {
            let out = s_
                .cmd(args)
                .env("VIBEKE_HERDR_BROKER", sock)
                .env("HERDR_SOCKET_PATH", sock)
                .output()
                .unwrap();
            assert!(!out.status.success(), "{sock:?} {args:?}");
        }
    }
    // Outside a plugin invocation a plain HERDR_SOCKET_PATH is ignored, and `server stop` is
    // never forwarded anywhere.
    let (code, e) = s_.fail(&["compat", "herdr", "server", "stop"]);
    assert_eq!(code, 1, "{e}");
    assert_eq!(e["error"]["kind"], "unsupported", "{e}");
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    t.join().unwrap();
    assert!(
        got.lock().unwrap().is_empty(),
        "the fake Herdr received: {:?}",
        got.lock().unwrap()
    );
}

// ---- finding 2: cross-session identity and pane session switching ------------------------------

#[test]
fn plugin_identity_across_sessions_is_authenticated_and_panes_cannot_switch() {
    let s_ = Session::new("");
    s_.json(&["workspace", "list"]);
    s_.json(&["--session", "work", "workspace", "list"]);
    let a = s_.path("src/a");
    let state = s_.path("state/plugins/state/acme.a");
    plugin(
        &a,
        r#"id = "acme.a"
[[actions]]
id = "imp"
title = "Impersonate"
command = ["sh", "bin/imp.sh"]
[[actions]]
id = "stale"
title = "Stale"
command = ["sh", "bin/stale.sh"]
"#,
        &[
            (
                "imp.sh",
                "HERDR_PLUGIN_ID=acme.b herdr --session work workspace create --cwd /tmp --label from-a > \"$HERDR_PLUGIN_STATE_DIR/imp.out\" 2> \"$HERDR_PLUGIN_STATE_DIR/imp.err\"\n",
            ),
            (
                "stale.sh",
                "sleep 3\nherdr --session work workspace list > \"$HERDR_PLUGIN_STATE_DIR/stale.out\" 2> \"$HERDR_PLUGIN_STATE_DIR/stale.err\"\necho $? > \"$HERDR_PLUGIN_STATE_DIR/stale.code\"\n",
            ),
        ],
    );
    s_.json(&["plugin", "link", a.to_str().unwrap(), "--yes"]);

    // A's action claims B's identity through the environment: it still acts as A.
    let log = s_.json(&["plugin", "action", "run", "acme.a.imp"]);
    let done = wait_finished(&s_, "acme.a", &s(&log["log"]["log_id"]));
    assert_eq!(
        done["status"],
        "succeeded",
        "{done} {}",
        read(&state.join("imp.err"))
    );
    let out: Value = serde_json::from_str(&read(&state.join("imp.out"))).unwrap_or(Value::Null);
    assert_eq!(out["type"], "workspace_created", "{out}");
    let calls = s_.json(&[
        "--session",
        "work",
        "api",
        "call",
        "events.read",
        &json!({"types": "plugin.api_call"}).to_string(),
    ]);
    let calls = calls["events"].as_array().cloned().unwrap_or_default();
    assert!(!calls.is_empty());
    assert!(
        calls.iter().all(|c| c["actor"]["id"] == "acme.a"),
        "audited as the authenticated plugin: {calls:?}"
    );

    // A stale invocation cannot use a renewed grant.
    s_.json(&["plugin", "action", "run", "acme.a.stale"]);
    std::thread::sleep(Duration::from_millis(500));
    s_.json(&["plugin", "untrust", "acme.a"]);
    s_.json(&["plugin", "trust", "acme.a", "--legacy"]);
    wait_for("stale action", 20000, || state.join("stale.code").exists());
    assert_ne!(read(&state.join("stale.code")).trim(), "0");
    assert!(
        read(&state.join("stale.err")).contains("permission_denied"),
        "{}",
        read(&state.join("stale.err"))
    );

    // A pane of the default session cannot switch sessions with its token removed, neither
    // through the shim nor by calling the destination directly.
    let created = s_.herdr(&["workspace", "create", "--cwd", "/tmp"]);
    let pane = s(&created["root_pane"]["pane_id"]);
    let bin = env!("CARGO_BIN_EXE_vibeke");
    let (m1, m2) = (s_.path("switch1.out"), s_.path("switch2.out"));
    let env = s_.shell_env();
    s_.json(&[
        "pane",
        "run",
        &pane,
        &format!(
            "env -u VIBEKE_PANE_TOKEN {env} {bin} compat herdr --session work workspace list 2> {m}; echo rc=$? >> {m}",
            m = m1.display()
        ),
    ]);
    s_.json(&[
        "pane",
        "run",
        &pane,
        &format!(
            "env -u VIBEKE_PANE_TOKEN {env} {bin} --json --session work api call compat.herdr.call '{{\"method\":\"workspace.list\"}}' > {m} 2>&1; echo rc=$? >> {m}",
            m = m2.display()
        ),
    ]);
    wait_for("switch attempts", 20000, || {
        read(&m1).contains("rc=") && read(&m2).contains("rc=")
    });
    let (o1, o2) = (read(&m1), read(&m2));
    assert!(
        o1.contains("permission_denied") && o1.contains("rc=5"),
        "{o1}"
    );
    assert!(
        o2.contains("another session") && !o2.contains("rc=0"),
        "{o2}"
    );
}

// ---- finding 3: broker lifetime and revocation -------------------------------------------------

fn write_holder(s_: &Session) -> PathBuf {
    let src = s_.path("src/life");
    plugin(
        &src,
        r#"id = "acme.life"
[[actions]]
id = "hold"
title = "Hold"
command = ["sh", "bin/hold.sh"]
"#,
        &[(
            "hold.sh",
            "echo \"$HERDR_SOCKET_PATH\" > \"$HERDR_PLUGIN_STATE_DIR/sock.$$\"\nsleep 4\n",
        )],
    );
    s_.json(&["plugin", "link", src.to_str().unwrap(), "--yes"]);
    s_.path("state/plugins/state/acme.life")
}

/// Start the `hold` action; returns its log id and broker socket.
fn start_hold(s_: &Session, state: &Path) -> (String, PathBuf) {
    let log = s_.json(&["plugin", "action", "run", "acme.life.hold"]);
    let id = s(&log["log"]["log_id"]);
    let f = state.join(format!("sock.{}", log["log"]["pid"]));
    wait_for("broker path", 15000, || read(&f).trim().ends_with(".sock"));
    (id, PathBuf::from(read(&f).trim()))
}

fn connect(sock: &Path) -> BufReader<UnixStream> {
    let st = UnixStream::connect(sock).unwrap();
    st.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    BufReader::new(st)
}

fn send(c: &mut BufReader<UnixStream>, req: Value) {
    let _ = c.get_mut().write_all(format!("{req}\n").as_bytes());
}

#[test]
fn closing_or_revoking_a_broker_cuts_held_connections_subscriptions_and_waits() {
    let s_ = Session::new("");
    let created = s_.herdr(&["workspace", "create", "--cwd", "/tmp"]);
    let ws = s(&created["workspace"]["workspace_id"]);
    let state = write_holder(&s_);

    // Connections opened while the action runs lose authority when it exits.
    let (id, sock) = start_hold(&s_, &state);
    let mut held = connect(&sock);
    let mut sub = connect(&sock);
    send(
        &mut sub,
        json!({"id": "s", "method": "events.subscribe", "params": {"subscriptions": [{"type": "tab.created"}]}}),
    );
    let ack = read_line(&mut sub).expect("subscription ack");
    assert_eq!(ack["result"]["type"], "subscription_started", "{ack}");
    let done = wait_finished(&s_, "acme.life", &id);
    assert_eq!(done["status"], "succeeded", "{done}");
    // Closing the broker closes the subscription itself, before any event arrives.
    // (macOS rejects the call with EINVAL once the peer has shut the socket down.)
    let _ = sub.get_ref().set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = String::new();
    let n = sub.read_line(&mut buf);
    assert!(
        matches!(n, Ok(0))
            || n.as_ref()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionReset),
        "subscription still open after the action exited: {n:?} {buf}"
    );
    send(
        &mut held,
        json!({"id": "1", "method": "workspace.create", "params": {"cwd": "/tmp", "label": "late"}}),
    );
    let r = read_line(&mut held);
    assert!(
        r.as_ref().is_none_or(|v| v.get("result").is_none()),
        "held connection still authorized: {r:?}"
    );
    s_.json(&["tab", "create", "--workspace", &ws]);
    let ev = read_line(&mut sub);
    assert!(ev.is_none(), "subscription survived the action: {ev:?}");
    let labels = s_.herdr(&["workspace", "list"]);
    assert!(
        !labels.to_string().contains("\"late\""),
        "late mutation applied: {labels}"
    );

    // Revocation during events.wait: no event is delivered.
    let (_, sock) = start_hold(&s_, &state);
    let mut w = connect(&sock);
    send(
        &mut w,
        json!({"id": "w", "method": "events.wait", "params": {"subscriptions": [{"type": "tab.created"}], "timeout_ms": 10000}}),
    );
    std::thread::sleep(Duration::from_millis(300));
    s_.json(&["plugin", "untrust", "acme.life"]);
    s_.json(&["tab", "create", "--workspace", &ws]);
    let r = read_line(&mut w);
    assert!(
        r.as_ref()
            .is_none_or(|v| v["error"]["code"] == "permission_denied"),
        "event delivered after revocation: {r:?}"
    );

    // Revoke + immediate re-grant does not revive an old connection.
    s_.json(&["plugin", "trust", "acme.life", "--legacy"]);
    let (_, sock) = start_hold(&s_, &state);
    let mut old = connect(&sock);
    s_.json(&["plugin", "untrust", "acme.life"]);
    s_.json(&["plugin", "trust", "acme.life", "--legacy"]);
    send(
        &mut old,
        json!({"id": "2", "method": "workspace.list", "params": {}}),
    );
    let r = read_line(&mut old);
    assert!(
        r.as_ref()
            .is_none_or(|v| v["error"]["code"] == "permission_denied"),
        "old connection revived by the new grant: {r:?}"
    );
}

// ---- finding 4: hook cache ---------------------------------------------------------------------

#[test]
fn a_warm_hook_cache_never_launches_changed_code() {
    let s_ = Session::new("");
    let created = s_.herdr(&["workspace", "create", "--cwd", "/tmp"]);
    let ws = s(&created["workspace"]["workspace_id"]);
    let src = s_.path("src/hook");
    let state = s_.path("state/plugins/state/acme.hook");
    plugin(
        &src,
        "id = \"acme.hook\"\n[[events]]\non = \"tab.created\"\ncommand = [\"sh\", \"bin/hook.sh\"]\n",
        &[(
            "hook.sh",
            "echo v1 >> \"$HERDR_PLUGIN_STATE_DIR/hooks.txt\"\n",
        )],
    );
    s_.json(&["plugin", "link", src.to_str().unwrap(), "--yes"]);
    let hooks = || read(&state.join("hooks.txt"));
    s_.json(&["tab", "create", "--workspace", &ws]);
    wait_for("first hook (cache warm)", 15000, || hooks().contains("v1"));

    // The referenced script changes; the next event comes well within the cache's 2 s.
    std::fs::write(
        src.join("bin/hook.sh"),
        "echo evil >> \"$HERDR_PLUGIN_STATE_DIR/hooks.txt\"\n",
    )
    .unwrap();
    s_.json(&["tab", "create", "--workspace", &ws]);
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!hooks().contains("evil"), "{}", hooks());

    // After a new review the new script runs; then a manifest edit stops it again.
    s_.json(&["plugin", "trust", "acme.hook", "--legacy"]);
    s_.json(&["tab", "create", "--workspace", &ws]);
    wait_for("reviewed hook", 15000, || hooks().contains("evil"));
    let n = hooks().lines().count();
    let mf = src.join("herdr-plugin.toml");
    std::fs::write(&mf, read(&mf) + "# edited\n").unwrap();
    s_.json(&["tab", "create", "--workspace", &ws]);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(hooks().lines().count(), n, "{}", hooks());
    let refused = s_
        .logs("acme.hook")
        .into_iter()
        .filter(|l| l["status"] == "failed" && s(&l["stderr"]).contains("not started"))
        .count();
    assert!(refused >= 1, "stale launches are recorded, not run");
}

// ---- findings 10 and 11: baseline request/response shapes --------------------------------------

#[test]
fn baseline_invocation_logs_split_targets_layout_root_and_split_paths() {
    let s_ = Session::new("[compat.herdr]\nenabled = true\n");
    let src = s_.path("src/x");
    plugin(
        &src,
        r#"id = "acme.x"
[[actions]]
id = "ok"
title = "Ok"
command = ["sh", "bin/ok.sh"]
[[actions]]
id = "bad"
title = "Bad"
command = ["sh", "-c", "exit 3"]
"#,
        &[("ok.sh", "echo fine\n")],
    );
    s_.json(&["plugin", "link", src.to_str().unwrap(), "--yes"]);
    let created = s_.herdr(&["workspace", "create", "--cwd", "/tmp"]);
    let p1 = s(&created["root_pane"]["pane_id"]);
    let tab = s(&created["tab"]["tab_id"]);
    let sock = s_.compat_socket();
    wait_for("compat socket", 5000, || sock.exists());

    // Qualified action_id without plugin_id.
    let v = raw(
        &sock,
        json!({"id": "1", "method": "plugin.action.invoke", "params": {"action_id": "acme.x.ok"}}),
    );
    assert_eq!(v["result"]["type"], "plugin_action_started", "{v}");
    let log = &v["result"]["log"];
    assert_eq!(log["command"], json!(["sh", "bin/ok.sh"]), "{log}");
    assert!(log["started_unix_ms"].as_i64().is_some(), "{log}");
    let ok = wait_finished(&s_, "acme.x", &s(&log["log_id"]));
    let v = raw(
        &sock,
        json!({"id": "2", "method": "plugin.action.invoke", "params": {"plugin_id": "acme.x", "action_id": "bad"}}),
    );
    let bad = wait_finished(&s_, "acme.x", &s(&v["result"]["log"]["log_id"]));
    let v = raw(
        &sock,
        json!({"id": "3", "method": "plugin.log.list", "params": {"plugin_id": "acme.x"}}),
    );
    assert_eq!(v["result"]["type"], "plugin_log_list");
    let logs = v["result"]["logs"].as_array().unwrap();
    let find = |id: &Value| logs.iter().find(|l| l["log_id"] == *id).unwrap().clone();
    let (ok, bad) = (find(&ok["log_id"]), find(&bad["log_id"]));
    assert_eq!(ok["status"], "succeeded", "{ok}");
    assert_eq!(ok["exit_code"], 0);
    assert!(ok["finished_unix_ms"].as_i64() >= ok["started_unix_ms"].as_i64());
    assert!(s(&ok["stdout"]).contains("fine"));
    assert_eq!(bad["status"], "failed", "{bad}");
    assert_eq!(bad["exit_code"], 3);
    for l in [&ok, &bad] {
        for k in [
            "log_id",
            "plugin_id",
            "action_id",
            "command",
            "status",
            "started_unix_ms",
            "stdout",
            "stderr",
        ] {
            assert!(l.get(k).is_some(), "{k} missing: {l}");
        }
    }

    // pane.split honours target_pane_id for an unfocused pane.
    let v = raw(
        &sock,
        json!({"id": "4", "method": "pane.split", "params": {"target_pane_id": p1, "direction": "right"}}),
    );
    let p2 = s(&v["result"]["pane"]["pane_id"]);
    assert!(!p2.is_empty(), "{v}");
    let v = raw(
        &sock,
        json!({"id": "5", "method": "pane.split", "params": {"target_pane_id": p2, "direction": "down"}}),
    );
    let p3 = s(&v["result"]["pane"]["pane_id"]);
    let v = raw(
        &sock,
        json!({"id": "6", "method": "layout.export", "params": {"tab_id": tab}}),
    );
    let root = &v["result"]["layout"]["root"];
    assert_eq!(root["first"]["pane_id"], p1.as_str(), "{root}");
    assert_eq!(root["second"]["type"], "split", "{root}");
    assert_eq!(root["second"]["first"]["pane_id"], p2.as_str(), "{root}");
    assert_eq!(root["second"]["second"]["pane_id"], p3.as_str(), "{root}");

    // Baseline layout.apply with `root`.
    let v = raw(
        &sock,
        json!({"id": "7", "method": "layout.apply", "params": {"tab_id": tab, "root": {
            "type": "split", "direction": "vertical", "ratio": 0.3,
            "first": {"type": "pane", "pane_id": p3},
            "second": {"type": "split", "direction": "horizontal", "ratio": 0.5,
                "first": {"type": "pane", "pane_id": p1},
                "second": {"type": "pane", "pane_id": p2}}}}}),
    );
    let root = &v["result"]["layout"]["root"];
    assert_eq!(root["first"]["pane_id"], p3.as_str(), "{v}");
    assert_eq!(root["direction"], "vertical");

    // set_split_ratio addresses the nested (unfocused) split by its baseline path.
    let v = raw(
        &sock,
        json!({"id": "8", "method": "layout.set_split_ratio", "params": {"tab_id": tab, "path": [true], "ratio": 0.25}}),
    );
    let root = &v["result"]["layout"]["root"];
    assert_eq!(root["second"]["ratio"], 0.25, "{v}");
    assert_eq!(root["ratio"], 0.3, "the root split is unchanged: {v}");
    let v = raw(
        &sock,
        json!({"id": "9", "method": "layout.set_split_ratio", "params": {"tab_id": tab, "path": ["x"], "ratio": 0.5}}),
    );
    assert_eq!(v["error"]["code"], "invalid_params", "{v}");
}
