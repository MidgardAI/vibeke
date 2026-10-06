//! Herdr compatibility, M5 slice 2, end to end against isolated Vibeke sessions: layout and
//! pane move/swap methods with their events, worktree methods and `worktree.*` events (also as
//! `[[events]]` hooks), the named-session socket layout and `--session`, plugin broker survival
//! across a server restart and the long-running rule, plugin panes, audit events and log
//! redaction, and copy-only migration from a fixture directory.
//!
//! Every test uses temp VIBEKE_* dirs; nothing touches a real Herdr, its config or sockets.

use serde_json::{Value, json};
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
            .prefix("vkm5b")
            .tempdir_in("/tmp")
            .unwrap();
        let config = config.replace("{root}", &dir.path().display().to_string());
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        Session { dir }
    }
    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().canonicalize().unwrap().join(p)
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
    fn herdr_fail(&self, args: &[&str]) -> (i32, Value) {
        let mut a = vec!["compat", "herdr"];
        a.extend(args);
        self.fail(&a)
    }
    fn events(&self, types: &str) -> Vec<Value> {
        let v = self.json(&[
            "api",
            "call",
            "events.read",
            &json!({"types": types}).to_string(),
        ]);
        v["events"].as_array().cloned().unwrap_or_default()
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

/// An `events.subscribe` stream on the compat listener.
struct Stream {
    rd: BufReader<UnixStream>,
}

impl Stream {
    fn open(sock: &Path, subs: Value) -> Self {
        let mut st = UnixStream::connect(sock).unwrap();
        st.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        let req =
            json!({"id": "s", "method": "events.subscribe", "params": {"subscriptions": subs}});
        st.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut rd = BufReader::new(st);
        let mut ack = String::new();
        rd.read_line(&mut ack).unwrap();
        let ack: Value = serde_json::from_str(&ack).unwrap();
        assert_eq!(ack["result"]["type"], "subscription_started", "{ack}");
        Stream { rd }
    }
    /// Read events until one named `name` arrives.
    fn until(&mut self, name: &str) -> Value {
        loop {
            let mut l = String::new();
            let n = self.rd.read_line(&mut l).expect("event stream timed out");
            assert!(n > 0, "event stream closed while waiting for {name}");
            let v: Value = serde_json::from_str(&l).unwrap();
            if v["event"] == name {
                return v;
            }
        }
    }
}

fn pane_ids(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(n: &Value, out: &mut Vec<String>) {
        match n["type"].as_str() {
            Some("pane") => out.push(s(&n["pane_id"])),
            Some("split") => {
                walk(&n["first"], out);
                walk(&n["second"], out);
            }
            _ => {}
        }
    }
    walk(v, &mut out);
    out
}

#[test]
fn layout_pane_move_swap_metadata_and_their_events() {
    let s_ = Session::new("[compat.herdr]\nenabled = true\n");
    let created = s_.herdr(&["workspace", "create", "--cwd", "/tmp", "--label", "lay"]);
    let ws = s(&created["workspace"]["workspace_id"]);
    let p1 = s(&created["root_pane"]["pane_id"]);
    let t1 = s(&created["tab"]["tab_id"]);
    let p2 = s(&s_.herdr(&["pane", "split", &p1, "--direction", "right"])["pane"]["pane_id"]);
    let sock = s_.path("run/herdr-compat/herdr.sock");
    wait_for("compat socket", 5000, || sock.exists());
    let mut ev = Stream::open(
        &sock,
        json!([
            {"type": "layout.updated"},
            {"type": "pane.moved"},
            {"type": "tab.moved"},
            {"type": "pane.output_matched", "pane_id": p1},
        ]),
    );

    // Export: a binary split tree with stable split ids and Herdr pane ids.
    let lay = s_.herdr(&["layout", "export", "--tab-id", &t1]);
    assert_eq!(lay["type"], "layout_snapshot", "{lay}");
    let root = &lay["layout"]["root"];
    assert_eq!(root["type"], "split");
    assert_eq!(root["split_id"], "s");
    assert_eq!(root["direction"], "horizontal");
    assert_eq!(pane_ids(root), vec![p1.clone(), p2.clone()]);
    let snap = s_.herdr(&["session", "snapshot"]);
    assert!(
        snap["layouts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["tab_id"] == t1.as_str()),
        "session.snapshot carries layouts: {snap}"
    );

    // Split ratio, then apply a rearranged snapshot (panes swapped, vertical).
    let r = s_.herdr(&["layout", "set-split-ratio", "s", "0.7", "--tab-id", &t1]);
    assert!(
        (r["layout"]["root"]["ratio"].as_f64().unwrap() - 0.7).abs() < 0.01,
        "{r}"
    );
    let e = ev.until("layout_updated");
    assert_eq!(e["data"]["tab_id"], t1.as_str());
    assert_eq!(e["data"]["layout"]["root"]["type"], "split", "{e}");
    let apply = json!({
        "tab_id": t1,
        "layout": {"root": {"type": "split", "direction": "vertical", "ratio": 0.4,
            "first": {"type": "pane", "pane_id": p2}, "second": {"type": "pane", "pane_id": p1}}}
    });
    let sock_req = |m: &str, p: &Value| -> Value {
        let mut st = UnixStream::connect(&sock).unwrap();
        st.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let req = json!({"id": "r", "method": m, "params": p});
        st.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut l = String::new();
        BufReader::new(st).read_line(&mut l).unwrap();
        serde_json::from_str(&l).unwrap()
    };
    let v = sock_req("layout.apply", &apply);
    assert_eq!(v["result"]["type"], "layout_snapshot", "{v}");
    assert_eq!(v["result"]["layout"]["root"]["direction"], "vertical");
    assert_eq!(
        pane_ids(&v["result"]["layout"]["root"]),
        vec![p2.clone(), p1.clone()]
    );
    let bad = json!({"tab_id": t1, "layout": {"root": {"type": "pane", "pane_id": p1}}});
    let v = sock_req("layout.apply", &bad);
    assert_eq!(v["error"]["code"], "invalid_params", "every pane once: {v}");

    // Swap and move (to a new tab): `pane.moved` events.
    let v = s_.herdr(&["pane", "swap", &p1, &p2]);
    assert_eq!(v["type"], "pane_list");
    let e = ev.until("pane_moved");
    assert!(e["data"]["pane_id"] == p1.as_str() || e["data"]["pane_id"] == p2.as_str());
    let tab2 = s_.herdr(&["tab", "create", "--workspace-id", &ws, "--label", "two"]);
    let t2 = s(&tab2["tab"]["tab_id"]);
    let moved = s_.herdr(&["pane", "move", &p2, "--tab-id", &t2, "--direction", "down"]);
    assert_eq!(moved["pane"]["tab_id"], t2.as_str(), "{moved}");
    assert_eq!(
        moved["pane"]["pane_id"],
        p2.as_str(),
        "ids are stable across a move"
    );
    loop {
        let e = ev.until("pane_moved");
        if e["data"]["pane_id"] == p2.as_str() && e["data"]["to_tab_id"] == t2.as_str() {
            assert_eq!(e["data"]["from_tab_id"], t1.as_str());
            break;
        }
    }
    let l2 = s_.herdr(&["layout", "export", "--tab-id", &t2]);
    assert!(pane_ids(&l2["layout"]["root"]).contains(&p2));
    let (_, e) = s_.herdr_fail(&["pane", "move", &p2]);
    assert_eq!(e["error"]["code"], "invalid_params", "{e}");

    // Tab move emits `tab.moved` (native event now).
    s_.herdr(&["tab", "move", &t2, "0"]);
    let e = ev.until("tab_moved");
    assert_eq!(e["data"]["tab_id"], t2.as_str());
    assert_eq!(e["data"]["insert_index"], 0);

    // Output matchers feed `pane.output_matched`; process info; metadata; window title.
    s_.herdr(&["pane", "run", &p1, "echo slice-$((1+1))-ok"]);
    let m = s_.herdr(&[
        "pane",
        "wait-output",
        &p1,
        "--match",
        "slice-2-ok",
        "--timeout-ms",
        "15000",
    ]);
    assert_eq!(m["type"], "pane_output_matched");
    let e = ev.until("pane_output_matched");
    assert_eq!(e["data"]["pane_id"], p1.as_str());
    let pi = s_.herdr(&["pane", "process-info", &p1]);
    assert_eq!(pi["type"], "pane_process_info", "{pi}");
    assert!(pi["pid"].as_u64().unwrap() > 0);
    assert_eq!(pi["pane_id"], p1.as_str());
    s_.herdr(&["pane", "report-metadata", &p1, "--ci", "green"]);
    let g = s_.herdr(&["pane", "get", &p1]);
    assert_eq!(g["pane"]["metadata"]["ci"], "green", "{g}");
    s_.herdr(&["workspace", "report-metadata", &ws, "--branch", "main"]);
    let w = s_.herdr(&["workspace", "get", &ws]);
    assert_eq!(w["workspace"]["metadata"]["branch"], "main", "{w}");
    let v = sock_req("client.window_title.set", &json!({"title": "build"}));
    assert_eq!(v["result"]["type"], "ok", "{v}");
    assert!(!s_.events("client.window_title_changed").is_empty());
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn worktree_create_open_and_worktree_events_reach_streams_and_hooks() {
    let s_ = Session::new(
        "[compat.herdr]\nenabled = true\n[tasks]\nroot = \"{root}/wt\"\nfetch_before_create = false\n",
    );
    let repo = s_.path("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "x\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    // A plugin hooking the two events 15 corpus plugins use.
    let src = s_.path("src/wt");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("herdr-plugin.toml"),
        "id = \"acme.wt\"\n\
         [[events]]\non = \"worktree.created\"\ncommand = [\"sh\", \"-c\", \"echo \\\"$HERDR_PLUGIN_EVENT $HERDR_PLUGIN_EVENT_JSON\\\" >> \\\"$HERDR_PLUGIN_STATE_DIR/ev.txt\\\"\"]\n\
         [[events]]\non = \"worktree.opened\"\ncommand = [\"sh\", \"-c\", \"echo \\\"$HERDR_PLUGIN_EVENT $HERDR_WORKSPACE_ID\\\" >> \\\"$HERDR_PLUGIN_STATE_DIR/ev.txt\\\"\"]\n",
    )
    .unwrap();
    s_.json(&["plugin", "link", src.to_str().unwrap()]);
    s_.json(&["plugin", "trust", "acme.wt", "--legacy"]);
    s_.json(&["workspace", "list"]);
    let sock = s_.path("run/herdr-compat/herdr.sock");
    wait_for("compat socket", 5000, || sock.exists());
    let mut ev = Stream::open(
        &sock,
        json!([{"type": "worktree.created"}, {"type": "worktree.opened"}]),
    );

    let r = s_.herdr(&[
        "worktree",
        "create",
        "--cwd",
        repo.to_str().unwrap(),
        "--branch",
        "feat/compat",
    ]);
    assert_eq!(r["type"], "worktree_created", "{r}");
    let path = s(&r["worktree"]["path"]);
    assert!(path.starts_with(s_.path("wt").to_str().unwrap()), "{r}");
    assert_eq!(r["worktree"]["branch"], "feat/compat");
    assert!(Path::new(&path).join("README").exists());
    let ws = s(&r["workspace"]["workspace_id"]);
    assert!(
        ws.starts_with('w'),
        "worktree.create opens a workspace: {r}"
    );
    let e = ev.until("worktree_created");
    assert_eq!(e["data"]["worktree"]["path"], path.as_str(), "{e}");
    assert_eq!(e["data"]["workspace_id"], ws.as_str());

    // Opening the same worktree reuses its workspace.
    let o = s_.herdr(&["worktree", "open", &path]);
    assert_eq!(o["type"], "worktree_opened", "{o}");
    assert_eq!(o["created"], false);
    assert_eq!(o["workspace"]["workspace_id"], ws.as_str());
    let e = ev.until("worktree_opened");
    assert_eq!(e["data"]["worktree"]["branch"], "feat/compat");

    let state = s_.path("state/plugins/state/acme.wt/ev.txt");
    wait_for("worktree hooks", 15000, || {
        let t = read(&state);
        t.contains("worktree.created") && t.contains("worktree.opened")
    });
    let t = read(&state);
    assert!(t.contains(&format!("worktree.opened {ws}")), "{t}");
    assert!(
        t.contains("feat/compat"),
        "event JSON reaches the hook: {t}"
    );

    let (_, e) = s_.herdr_fail(&["worktree", "open", "/tmp/definitely-not-a-worktree"]);
    assert_eq!(e["error"]["code"], "worktree_not_found", "{e}");
    let (_, e) = s_.herdr_fail(&[
        "worktree",
        "create",
        "--cwd",
        repo.to_str().unwrap(),
        "--branch",
        "feat/compat",
    ]);
    assert_eq!(e["error"]["code"], "conflict", "branch in use: {e}");
}

#[test]
fn named_sessions_use_herdrs_socket_layout_and_session_selection() {
    let s_ = Session::new("[compat.herdr]\nenabled = true\n");
    s_.json(&["workspace", "list"]);
    s_.json(&["--session", "work", "workspace", "list"]);
    let def = s_.path("run/herdr-compat/herdr.sock");
    let work = s_.path("run/herdr-compat/sessions/work/herdr.sock");
    wait_for("both listeners", 5000, || def.exists() && work.exists());

    // `herdr --session` and HERDR_SESSION select the Vibeke session of the same name.
    s_.herdr(&[
        "--session",
        "work",
        "workspace",
        "create",
        "--cwd",
        "/tmp",
        "--label",
        "inwork",
    ]);
    let labels = |v: &Value| -> Vec<String> {
        v["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| s(&w["label"]))
            .collect()
    };
    assert!(labels(&s_.herdr(&["--session=work", "workspace", "list"])).contains(&"inwork".into()));
    assert!(!labels(&s_.herdr(&["workspace", "list"])).contains(&"inwork".into()));
    let out = s_
        .cmd(&["compat", "herdr", "workspace", "list"])
        .env("HERDR_SESSION", "work")
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(labels(&v).contains(&"inwork".into()), "{v}");

    // An explicitly selected session is never spawned.
    let (code, e) = s_.herdr_fail(&["--session", "nope", "workspace", "list"]);
    assert_eq!(code, 4, "no server: {e}");
    assert!(!s_.path("run/nope/vibeke.sock").exists());
    let (code, _) = s_.herdr_fail(&["--session", "../x", "workspace", "list"]);
    assert_eq!(code, 2);

    // A pane cannot switch sessions.
    let created = s_.herdr(&["workspace", "create", "--cwd", "/tmp"]);
    let pane = s(&created["root_pane"]["pane_id"]);
    let bin = env!("CARGO_BIN_EXE_vibeke");
    let marker = s_.path("switch.out");
    s_.herdr(&[
        "pane",
        "run",
        &pane,
        &format!(
            "{bin} compat herdr --session work workspace list 2> {m}; echo rc=$? >> {m}",
            m = marker.display()
        ),
    ]);
    wait_for("session switch attempt", 15000, || {
        read(&marker).contains("rc=")
    });
    let out = read(&marker);
    assert!(
        out.contains("permission_denied") && out.contains("rc=5"),
        "{out}"
    );

    // The named session's socket and directory go away on a clean stop.
    s_.json(&["--session", "work", "server", "stop", "--kill-panes"]);
    wait_for("named socket removed", 10000, || !work.exists());
    assert!(def.exists());
}

fn write_daemon_plugin(dir: &Path) {
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(
        dir.join("herdr-plugin.toml"),
        r#"id = "acme.daemon"
[[startup]]
command = ["sh", "bin/daemon.sh"]

[[actions]]
id = "bg"
title = "Leave a child behind"
command = ["sh", "bin/bg.sh"]

[[actions]]
id = "leak"
title = "Print a credential"
command = ["sh", "bin/leak.sh"]

[[panes]]
id = "viewer"
title = "Viewer"
placement = "split"
command = ["sh", "bin/viewer.sh"]

[[panes]]
id = "pop"
title = "Popup"
placement = "popup"
command = ["sh", "bin/viewer.sh"]
"#,
    )
    .unwrap();
    // The startup hook exits at once and leaves a child: its broker follows the process group.
    std::fs::write(
        dir.join("bin/daemon.sh"),
        "S=\"$HERDR_PLUGIN_STATE_DIR\"\n\
         [ -f \"$S/go\" ] || exit 0\n\
         rm -f \"$S/go\"\n\
         ( sleep 1; herdr ping > \"$S/d1\" 2>&1\n\
           while [ ! -f \"$S/restarted\" ]; do sleep 0.2; done\n\
           sleep 1.5; herdr workspace list > \"$S/d2\" 2> \"$S/d2.err\"; echo $? > \"$S/d2.code\" ) &\n\
         echo started\n",
    )
    .unwrap();
    // An action's child outlives the action: it loses the broker (no long-running declaration).
    std::fs::write(
        dir.join("bin/bg.sh"),
        "S=\"$HERDR_PLUGIN_STATE_DIR\"\n\
         ( sleep 2; herdr ping > \"$S/a1\" 2> \"$S/a1.err\"; echo $? > \"$S/a1.code\" ) &\n\
         echo spawned\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("bin/leak.sh"),
        "echo \"token ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\"\n\
         echo \"AWS key AKIAABCDEFGHIJKLMNOP\" >&2\n\
         herdr pane rename \"$HERDR_PANE_ID\" leaked > /dev/null\n\
         herdr pane list > /dev/null\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("bin/viewer.sh"),
        "S=\"$HERDR_PLUGIN_STATE_DIR\"\n\
         env | grep '^HERDR_' | sort > \"$S/viewer-env.txt\"\n\
         herdr pane current > \"$S/viewer-pane.json\" 2>&1\n\
         sleep 60\n",
    )
    .unwrap();
}

fn stop(s_: &Session) {
    let _ = s_.cmd(&["server", "stop"]).output();
    wait_for("server to stop", 10000, || {
        s_.cmd(&["--no-spawn", "server", "status"])
            .output()
            .is_ok_and(|o| !o.status.success())
    });
}

fn logs(s_: &Session) -> Vec<Value> {
    s_.json(&["plugin", "logs", "--plugin", "acme.daemon"])["logs"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn brokers_survive_a_restart_only_for_live_long_running_invocations() {
    let s_ = Session::new("");
    let src = s_.path("src/daemon");
    write_daemon_plugin(&src);
    let st = s_.path("state/plugins/state/acme.daemon");
    std::fs::create_dir_all(&st).unwrap();
    s_.json(&["plugin", "link", src.to_str().unwrap()]);
    s_.json(&["plugin", "trust", "acme.daemon", "--legacy"]);
    std::fs::write(st.join("go"), "").unwrap();
    s_.json(&["workspace", "list"]);

    // The startup process exited at once; its child still reaches the broker.
    wait_for("daemon child callback", 15000, || {
        read(&st.join("d1")).contains("pong")
    });
    let startup_log = logs(&s_)
        .into_iter()
        .find(|l| l["source"] == "startup")
        .unwrap();
    assert_eq!(startup_log["status"], "succeeded", "{startup_log}");
    assert!(s(&startup_log["stdout"]).contains("started"));
    let status = s_.json(&["compat", "status"]);
    assert_eq!(status["brokers"], 1, "the group keeps its broker: {status}");

    // An action's leftover child loses authority when the action exits.
    s_.json(&["plugin", "action", "run", "acme.daemon", "bg"]);
    wait_for("action child", 15000, || st.join("a1.code").exists());
    assert_ne!(read(&st.join("a1.code")).trim(), "0");
    assert!(
        read(&st.join("a1.err")).contains("permission_denied"),
        "{}",
        read(&st.join("a1.err"))
    );

    // Restart the server: the live startup binding is re-issued at the same path.
    let brokers_before: Value =
        serde_json::from_str(&read(&s_.path("run/default/herdr-compat/brokers.json"))).unwrap();
    let path_before = s(&brokers_before[0]["path"]);
    stop(&s_);
    s_.json(&["workspace", "list"]);
    // Re-issued at the same path, bound to the same grant, while the group is alive.
    wait_for("broker re-issued", 10000, || {
        let after: Value =
            serde_json::from_str(&read(&s_.path("run/default/herdr-compat/brokers.json")))
                .unwrap_or(Value::Null);
        after
            .as_array()
            .is_some_and(|a| a.iter().any(|b| b["path"] == path_before.as_str()))
    });
    assert!(Path::new(&path_before).exists());
    std::fs::write(st.join("restarted"), "").unwrap();
    wait_for("callback after restart", 15000, || {
        st.join("d2.code").exists()
    });
    assert_eq!(
        read(&st.join("d2.code")).trim(),
        "0",
        "{}",
        read(&st.join("d2.err"))
    );
    assert_eq!(
        serde_json::from_str::<Value>(&read(&st.join("d2"))).unwrap()["type"],
        "workspace_list"
    );
    // Logs survive the restart too.
    assert!(
        logs(&s_)
            .iter()
            .any(|l| l["log_id"] == startup_log["log_id"])
    );
    // Once the group is gone, so is the broker.
    wait_for("broker closed with its group", 15000, || {
        s_.json(&["compat", "status"])["brokers"] == 0
    });
}

#[test]
fn plugin_panes_audit_events_and_redacted_logs() {
    let s_ = Session::new("");
    let src = s_.path("src/daemon");
    write_daemon_plugin(&src);
    let st = s_.path("state/plugins/state/acme.daemon");
    s_.json(&["plugin", "link", src.to_str().unwrap()]);
    s_.json(&["plugin", "trust", "acme.daemon", "--legacy"]);
    let created = s_.herdr(&["workspace", "create", "--cwd", "/tmp"]);
    let pane = s(&created["root_pane"]["pane_id"]);

    // Credentials in plugin output are redacted; mutating callbacks are audited.
    let r = s_.json(&[
        "plugin",
        "action",
        "run",
        "acme.daemon",
        "leak",
        "--pane",
        &pane,
    ]);
    let id = s(&r["log"]["log_id"]);
    let mut log = Value::Null;
    wait_for("leak action", 15000, || {
        log = logs(&s_)
            .into_iter()
            .find(|l| l["log_id"] == id.as_str())
            .unwrap_or(Value::Null);
        log["status"] == "succeeded"
    });
    let out = s(&log["stdout"]) + &s(&log["stderr"]);
    assert!(out.contains("token"), "{log}");
    assert!(!out.contains("ghp_AAAAAAAA"), "redacted: {log}");
    assert!(!out.contains("AKIAABCDEFGHIJKLMNOP"), "redacted: {log}");
    let calls = s_.events("plugin.api_call");
    let methods: Vec<String> = calls.iter().map(|e| s(&e["data"]["method"])).collect();
    assert!(methods.contains(&"pane.rename".to_string()), "{calls:?}");
    assert!(
        !methods.contains(&"pane.list".to_string()),
        "reads are not audited"
    );
    let c = calls
        .iter()
        .find(|e| e["data"]["method"] == "pane.rename")
        .unwrap();
    assert_eq!(c["actor"]["kind"], "plugin");
    assert_eq!(c["actor"]["id"], "acme.daemon");
    assert_eq!(c["actor"]["invocation"], id.as_str());
    assert!(c["data"].get("params").is_none(), "metadata only");
    let started = s_.events("plugin.invocation_started");
    assert!(started.iter().any(|e| e["data"]["log_id"] == id.as_str()));
    wait_for("finished audit", 5000, || {
        s_.events("plugin.invocation_finished")
            .iter()
            .any(|e| e["data"]["log_id"] == id.as_str() && e["data"]["exit_code"] == 0)
    });

    // Plugin pane (split placement): its own broker, HERDR_* identity of the new pane.
    let opened = s_.herdr(&[
        "plugin",
        "pane",
        "open",
        "--plugin",
        "acme.daemon",
        "--entrypoint",
        "viewer",
        "--pane-id",
        &pane,
    ]);
    assert_eq!(opened["type"], "plugin_pane_opened", "{opened}");
    let vp = s(&opened["pane"]["pane_id"]);
    assert!(vp.starts_with("w") && vp != pane, "{opened}");
    wait_for("viewer pane callback", 15000, || {
        read(&st.join("viewer-pane.json")).contains("pane_info")
    });
    let env = read(&st.join("viewer-env.txt"));
    assert!(env.contains(&format!("HERDR_PANE_ID={vp}")), "{env}");
    assert!(env.contains("HERDR_PLUGIN_ENTRYPOINT_ID=viewer"), "{env}");
    assert!(env.contains("/herdr-compat/brokers/"), "{env}");
    let cur: Value = serde_json::from_str(&read(&st.join("viewer-pane.json"))).unwrap();
    assert_eq!(cur["pane"]["pane_id"], vp.as_str(), "{cur}");
    // Only the pane's broker is left (the startup hook's group and the action have ended).
    wait_for("only the pane broker", 10000, || {
        s_.json(&["compat", "status"])["brokers"] == 1
    });
    let f = s_.herdr(&[
        "plugin",
        "pane",
        "focus",
        "--plugin",
        "acme.daemon",
        "--entrypoint",
        "viewer",
    ]);
    assert_eq!(f["pane"]["pane_id"], vp.as_str());
    s_.herdr(&[
        "plugin",
        "pane",
        "close",
        "--plugin",
        "acme.daemon",
        "--entrypoint",
        "viewer",
    ]);
    wait_for("plugin pane gone with its broker", 15000, || {
        s_.json(&["compat", "status"])["brokers"] == 0
    });
    let (_, e) = s_.herdr_fail(&[
        "plugin",
        "pane",
        "open",
        "--plugin",
        "acme.daemon",
        "--entrypoint",
        "pop",
        "--pane-id",
        &pane,
    ]);
    assert_eq!(
        e["error"]["code"], "unsupported",
        "popups need the TUI: {e}"
    );
    let (_, e) = s_.herdr_fail(&[
        "plugin",
        "pane",
        "open",
        "--plugin",
        "acme.daemon",
        "--entrypoint",
        "nope",
    ]);
    assert_eq!(e["error"]["code"], "plugin_pane_not_found", "{e}");

    // Registry methods over the socket path (the shim's server transport).
    let r = s_.json(&[
        "api",
        "call",
        "compat.herdr.call",
        &json!({"method": "plugin.disable", "params": {"plugin_id": "acme.daemon"}}).to_string(),
    ]);
    assert_eq!(r["result"]["plugin"]["status"], "disabled", "{r}");
    let r = s_.json(&[
        "api",
        "call",
        "compat.herdr.call",
        &json!({"method": "plugin.enable", "params": {"plugin_id": "acme.daemon"}}).to_string(),
    ]);
    assert_eq!(r["result"]["plugin"]["status"], "active", "{r}");

    // Agent methods over a self-reported agent (Herdr's integration path).
    s_.herdr(&[
        "pane",
        "report-agent",
        &pane,
        "--agent",
        "claude",
        "--state",
        "idle",
    ]);
    let agents = s_.herdr(&["agent", "list"]);
    let target = s(&agents["agents"][0]["agent_id"]);
    assert!(!target.is_empty(), "{agents}");
    let r = s_.herdr(&["agent", "rename", &target, "helper"]);
    assert_eq!(r["agent"]["name"], "helper", "{r}");
    let w = s_.herdr(&[
        "agent",
        "wait",
        "helper",
        "--status",
        "idle",
        "--timeout-ms",
        "5000",
    ]);
    assert_eq!(w["agent_status"], "idle", "{w}");
    let (_, e) = s_.herdr_fail(&["agent", "wait", "helper", "--status", "sleepy"]);
    assert_eq!(e["error"]["code"], "invalid_params");
    let (_, e) = s_.herdr_fail(&["agent", "start"]);
    assert_eq!(e["error"]["code"], "invalid_params");
}

#[test]
fn migration_copies_from_an_explicit_fixture_dir_and_rolls_back() {
    let s_ = Session::new("");
    let h = s_.path("herdr-copy");
    let plug = s_.path("src/notes");
    std::fs::create_dir_all(&plug).unwrap();
    std::fs::write(plug.join("herdr-plugin.toml"), "id = \"acme.notes\"\n").unwrap();
    std::fs::create_dir_all(h.join("plugins/acme.notes/config")).unwrap();
    std::fs::create_dir_all(h.join("plugins/acme.notes/state")).unwrap();
    std::fs::write(h.join("plugins/acme.notes/config/settings.toml"), "a = 1\n").unwrap();
    std::fs::write(h.join("plugins/acme.notes/state/db.json"), "[]").unwrap();
    std::fs::write(
        h.join("plugins.json"),
        json!({"plugins": {"acme.notes": {"root": plug, "enabled": true}}}).to_string(),
    )
    .unwrap();
    let before = read(&h.join("plugins.json"));

    let (code, _) = s_.fail(&["plugin", "migrate"]);
    assert_eq!(code, 2, "no implicit source");
    let dry = s_.json(&[
        "plugin",
        "migrate",
        "--from",
        h.to_str().unwrap(),
        "--dry-run",
    ]);
    assert_eq!(dry["summary"]["copy"], 2, "{dry}");
    let cfg = s_.path("plugins/acme.notes/settings.toml");
    assert!(!cfg.exists(), "dry run writes nothing");

    let r = s_.json(&["plugin", "migrate", "--from", h.to_str().unwrap(), "--link"]);
    assert_eq!(r["summary"]["copy"], 2, "{r}");
    assert_eq!(r["linked"], json!(["acme.notes"]));
    assert_eq!(read(&cfg), "a = 1\n");
    let state = s_.path("state/plugins/state/acme.notes/db.json");
    assert_eq!(read(&state), "[]");
    assert!(
        h.join("plugins/acme.notes/state/db.json").exists(),
        "copied, not moved"
    );
    assert_eq!(read(&h.join("plugins.json")), before, "source untouched");
    let l = s_.json(&["plugin", "list"]);
    assert_eq!(
        l["plugins"][0]["status"], "untrusted",
        "linked, never trusted: {l}"
    );

    // A second run reports identical files; a changed destination is a conflict, kept.
    std::fs::write(&state, "[1]").unwrap();
    let r2 = s_.json(&[
        "plugin",
        "migrate",
        "--from",
        h.to_str().unwrap(),
        "--dry-run",
    ]);
    assert_eq!(r2["summary"]["conflict"], 1, "{r2}");
    assert_eq!(r2["summary"]["same"], 1);

    let rb = s_.json(&["plugin", "migrate", "--rollback"]);
    assert!(!cfg.exists(), "{rb}");
    assert_eq!(read(&state), "[1]", "changed files are kept on rollback");
    assert!(h.join("plugins/acme.notes/config/settings.toml").exists());
}
