//! Plugin system completion (07 §7.6, §7.7), end to end against isolated sessions:
//! `owner/repo[@ref]` installs from `file://` repositories (never the network), manifest default
//! key bindings, per-plugin concurrency and log ring limits, `agent.view.set/clear`, and
//! restricted (sandboxed) legacy mode.
//!
//! Every test uses temp VIBEKE_* dirs; nothing touches a real Herdr, its config or sockets.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct Session {
    dir: tempfile::TempDir,
    env: Vec<(String, String)>,
}

impl Session {
    fn new(config: &str) -> Self {
        Self::with_env(config, &[])
    }
    fn with_env(config: &str, env: &[(&str, &str)]) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkpe")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        Session {
            dir,
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
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
            .env("GITHUB_TOKEN", "ghp_not_for_plugins")
            .env(
                "VIBEKE_NOTIFIER",
                format!("log:{}", d.join("notes.jsonl").display()),
            );
        for (k, v) in &self.env {
            c.env(k, v);
        }
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
    fn fail(&self, args: &[&str]) -> (i32, String) {
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
    fn herdr(&self, args: &[&str]) -> Value {
        let mut a = vec!["compat", "herdr"];
        a.extend(args);
        self.json(&a)
    }
    fn api(&self, method: &str, params: Value) -> Value {
        self.json(&["api", "call", method, &params.to_string()])
    }
    fn plugin(&self, id: &str) -> Value {
        let l = self.api("plugin.list", json!({}));
        l["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["plugin_id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("{id} not listed: {l}"))
    }
    fn events(&self, types: &str) -> Vec<Value> {
        let v = self.api("events.read", json!({"types": types}));
        v["events"].as_array().cloned().unwrap_or_default()
    }
    /// Run an action and return the log record at launch.
    fn run(&self, plugin: &str, action: &str) -> Value {
        self.api(
            "plugin.action.run",
            json!({"plugin": plugin, "action": action}),
        )["log"]
            .clone()
    }
    /// Wait until `plugin` has no running invocation; returns its records (oldest first).
    fn settle(&self, plugin: &str) -> Vec<Value> {
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

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn git(dir: &Path, args: &[&str]) -> String {
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

// ---- owner/repo sources --------------------------------------------------------------------

#[test]
fn repository_installs_pin_the_commit_and_updates_need_a_new_trust_decision() {
    let base = tempfile::Builder::new()
        .prefix("vkgit")
        .tempdir_in("/tmp")
        .unwrap();
    let b = base.path().canonicalize().unwrap();
    let manifest = |v: &str| {
        format!(
            "id = \"acme.tool\"\nversion = \"{v}\"\n[[actions]]\nid = \"go\"\ntitle = \"Go\"\ncommand = [\"sh\", \"-c\", \"echo hi\"]\n"
        )
    };
    let work = b.join("work");
    write(&work.join("herdr-plugin.toml"), &manifest("1"));
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["add", "."]);
    git(&work, &["commit", "-qm", "one"]);
    git(&work, &["tag", "v1"]);
    let c1 = git(&work, &["rev-parse", "HEAD"]);
    let bare = b.join("srv/acme/tool");
    std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
    git(
        &b,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let base_url = format!("file://{}", b.join("srv").display());
    let s_ = Session::with_env(
        "",
        &[
            ("VIBEKE_TEST_HOOKS", "1"),
            ("VIBEKE_PLUGIN_GIT_BASE", &base_url),
        ],
    );

    // `owner/repo@ref`, resolved to a commit and recorded; inactive until trusted.
    let r = s_.json(&["plugin", "install", "acme/tool@v1"]);
    let p = &r["plugin"];
    assert_eq!(p["origin"]["kind"], "git", "{r}");
    assert_eq!(p["origin"]["commit"], c1.as_str(), "{r}");
    assert_eq!(p["origin"]["requested_ref"], "v1");
    assert_eq!(p["origin"]["repo"], "acme/tool");
    assert_eq!(p["status"], "untrusted");
    assert!(r["trust_terms"].as_str().unwrap().contains(&c1));
    let root = PathBuf::from(p["root"].as_str().unwrap());
    assert!(!root.join(".git").exists());
    let t = s_.json(&["plugin", "trust", "acme.tool", "--legacy"]);
    assert_eq!(
        t["grant"]["commit"],
        c1.as_str(),
        "the grant pins the commit"
    );
    assert_eq!(s_.plugin("acme.tool")["status"], "active");

    // `--ref` spelling, and `plugin update` (same recorded ref): same commit, still trusted.
    let r = s_.json(&["plugin", "install", "acme/tool", "--ref", "v1"]);
    assert_eq!(r["plugin"]["status"], "active", "{r}");
    let r = s_.json(&["plugin", "update", "acme.tool"]);
    assert_eq!(r["plugin"]["status"], "active", "{r}");

    // Upstream moves on: the default branch is a new commit with a new manifest. Installing it
    // leaves the plugin inactive until the operator reviews it again.
    write(&work.join("herdr-plugin.toml"), &manifest("2"));
    git(&work, &["commit", "-qam", "two"]);
    git(&work, &["push", "-q", bare.to_str().unwrap(), "main"]);
    let c2 = git(&work, &["rev-parse", "HEAD"]);
    let r = s_.json(&["plugin", "install", "acme/tool"]);
    assert_eq!(r["plugin"]["origin"]["commit"], c2.as_str(), "{r}");
    assert_eq!(
        r["plugin"]["status"], "untrusted",
        "an update re-asks trust"
    );
    let t = s_.json(&["plugin", "trust", "acme.tool", "--legacy"]);
    assert_eq!(t["grant"]["commit"], c2.as_str());
    assert_eq!(s_.plugin("acme.tool")["status"], "active");
    let l = s_.plugin("acme.tool");
    assert_eq!(
        l["origin"]["commit"],
        c2.as_str(),
        "plugin.list carries the origin"
    );

    // `--yes` accepts the displayed terms for a fresh install of a sha.
    let r = s_.json(&["plugin", "install", "acme/tool", "--ref", &c1, "--yes"]);
    assert_eq!(r["plugin"]["origin"]["commit"], c1.as_str());
    assert_eq!(r["plugin"]["status"], "active", "{r}");

    // Failures name the problem and register nothing new.
    let (_, e) = s_.fail(&["plugin", "install", "acme/missing"]);
    assert!(e.contains("fetch_failed"), "{e}");
    let (_, e) = s_.fail(&["plugin", "install", "acme/tool@nosuchref"]);
    assert!(e.contains("not found"), "{e}");
    let (_, e) = s_.fail(&["plugin", "install", "acme/tool@a", "--ref", "b"]);
    assert!(e.contains("disagree"), "{e}");
    // A dry run fetches, shows the commit and registers nothing.
    let d = s_.json(&["plugin", "install", "acme/tool@v1", "--dry-run"]);
    assert_eq!(d["commit"], c1.as_str(), "{d}");
}

// ---- manifest default key bindings ---------------------------------------------------------

fn write_keys_plugin(dir: &Path) {
    write(
        &dir.join("herdr-plugin.toml"),
        r#"id = "acme.keys"
version = "1"

[[actions]]
id = "go"
title = "Go"
command = ["sh", "-c", "true"]

[[keys.command]]
key = "prefix+alt+k"
type = "plugin_action"
command = "go"
description = "Go"

[[keys.command]]
key = "prefix+alt+j"
type = "plugin_action"
command = "acme.keys.go"

[[keys.command]]
key = "prefix+s"
type = "plugin_action"
command = "go"

[[keys.command]]
key = "prefix+alt+q"
type = "shell"
command = "ls"
"#,
    );
}

fn key<'a>(p: &'a Value, k: &str) -> &'a Value {
    p["keybindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["key"] == k)
        .unwrap_or_else(|| panic!("no binding {k} in {p}"))
}

#[test]
fn manifest_key_bindings_follow_trust_and_never_override_the_users_keys() {
    // The user bound prefix+alt+j to `help`; prefix+s is the default `settings` key.
    let s_ = Session::new("[keys]\nhelp = \"prefix+alt+j\"\n");
    let src = s_.path("src/keys");
    write_keys_plugin(&src);
    s_.json(&["plugin", "link", src.to_str().unwrap()]);

    // Untrusted: nothing is installed.
    let p = s_.plugin("acme.keys");
    assert_eq!(key(&p, "prefix+alt+k")["installed"], false);
    assert_eq!(key(&p, "prefix+alt+k")["reason"], "untrusted");

    s_.json(&["plugin", "trust", "acme.keys", "--legacy"]);
    let p = s_.plugin("acme.keys");
    let k = key(&p, "prefix+alt+k");
    assert_eq!(k["installed"], true, "{p}");
    assert_eq!(k["action"], "acme.keys.go", "bare action ids are qualified");
    let j = key(&p, "prefix+alt+j");
    assert_eq!(
        (&j["installed"], &j["reason"]),
        (&json!(false), &json!("conflict"))
    );
    assert_eq!(j["conflicts_with"], "help", "the user's key wins");
    assert_eq!(key(&p, "prefix+s")["conflicts_with"], "settings");
    assert_eq!(
        p["keybindings"].as_array().unwrap().len(),
        3,
        "shell bindings are not plugin's"
    );

    // The client's list carries the same bindings.
    let l = s_.api("plugin.action.list", json!({}));
    let installed: Vec<&Value> = l["keybindings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|k| k["installed"] == true)
        .collect();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0]["key"], "prefix+alt+k");

    // Disable: removed, and clients are told (CLI notifies the running server).
    s_.json(&["plugin", "disable", "acme.keys"]);
    let p = s_.plugin("acme.keys");
    assert_eq!(key(&p, "prefix+alt+k")["installed"], false);
    assert_eq!(key(&p, "prefix+alt+k")["reason"], "disabled");
    assert!(
        !s_.events("plugin.registry_changed").is_empty(),
        "registry changes are announced"
    );
    s_.json(&["plugin", "enable", "acme.keys"]);
    assert_eq!(
        key(&s_.plugin("acme.keys"), "prefix+alt+k")["installed"],
        true
    );

    // Two plugins wanting one chord: the first registered keeps it.
    let other = s_.path("src/keys2");
    write(
        &other.join("herdr-plugin.toml"),
        "id = \"acme.zkeys\"\n[[actions]]\nid = \"x\"\ncommand = [\"true\"]\n[[keys.command]]\nkey = \"prefix+alt+k\"\ntype = \"plugin_action\"\ncommand = \"x\"\n",
    );
    s_.json(&["plugin", "link", other.to_str().unwrap()]);
    s_.json(&["plugin", "trust", "acme.zkeys", "--legacy"]);
    let z = s_.plugin("acme.zkeys");
    assert_eq!(
        key(&z, "prefix+alt+k")["conflicts_with"],
        "acme.keys.go",
        "{z}"
    );

    // Unlink: gone.
    s_.json(&["plugin", "unlink", "acme.keys"]);
    let l = s_.api("plugin.action.list", json!({}));
    assert!(
        l["keybindings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|k| k["plugin_id"] != "acme.keys"),
        "{l}"
    );
    assert_eq!(
        key(&s_.plugin("acme.zkeys"), "prefix+alt+k")["installed"],
        true
    );
}

// ---- concurrency and log limits ------------------------------------------------------------

#[test]
fn per_plugin_concurrency_and_log_rings_are_enforced() {
    let s_ = Session::new(
        "[plugins.\"acme.lim\"]\nmax_concurrent = 1\nlog_max_lines = 20\nlog_max_records = 3\n",
    );
    let src = s_.path("src/lim");
    write(
        &src.join("herdr-plugin.toml"),
        r#"id = "acme.lim"
[[actions]]
id = "slow"
command = ["sh", "-c", "sleep 3"]
[[actions]]
id = "chatty"
command = ["sh", "-c", "i=0; while [ $i -lt 500 ]; do echo line$i; i=$((i+1)); done; echo fin >&2"]
"#,
    );
    s_.json(&["plugin", "link", src.to_str().unwrap()]);
    s_.json(&["plugin", "trust", "acme.lim", "--legacy"]);
    assert_eq!(s_.plugin("acme.lim")["max_concurrent"], 1);

    // The second invocation of the plugin is refused while the first runs, whatever action.
    let first = s_.run("acme.lim", "slow");
    assert_eq!(first["status"], "running");
    let (_, e) = s_.fail(&[
        "api",
        "call",
        "plugin.action.run",
        &json!({"plugin": "acme.lim", "action": "chatty"}).to_string(),
    ]);
    assert!(e.contains("running invocations"), "{e}");
    let logs = s_.settle("acme.lim");
    assert_eq!(logs.len(), 1, "a refused run leaves no record");
    // The slot is free again once it ends.
    s_.run("acme.lim", "chatty");
    let logs = s_.settle("acme.lim");
    let last = logs.last().unwrap();
    assert_eq!(last["status"], "succeeded", "{last}");
    let out = last["stdout"].as_str().unwrap();
    assert!(
        out.starts_with("[vibeke: ") && out.contains("earlier bytes of output truncated]"),
        "{out}"
    );
    assert!(out.trim_end().ends_with("line499"));
    assert!(
        out.lines().count() <= 21,
        "line ring: {}",
        out.lines().count()
    );
    assert!(last["stdout_truncated_bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        last["stderr"], "fin\n",
        "an untruncated stream has no marker"
    );
    // Records are capped per plugin.
    for _ in 0..4 {
        s_.run("acme.lim", "chatty");
        s_.settle("acme.lim");
    }
    assert_eq!(s_.settle("acme.lim").len(), 3, "log_max_records");
}

// ---- agent views ---------------------------------------------------------------------------

#[test]
fn plugins_set_and_clear_agent_views_scoped_to_their_grant() {
    let s_ = Session::new("");
    let src = s_.path("src/view");
    write(
        &src.join("herdr-plugin.toml"),
        r#"id = "acme.view"
[[actions]]
id = "set"
command = ["sh", "-c", "herdr agent view-set \"$(cat \"$HERDR_PLUGIN_STATE_DIR/target\")\" --text building --detail 'step 2 of 5' --tone warn > \"$HERDR_PLUGIN_STATE_DIR/set.out\" 2>&1"]
[[actions]]
id = "long"
command = ["sh", "-c", "herdr agent view-set \"$(cat \"$HERDR_PLUGIN_STATE_DIR/target\")\" --text \"$(printf 'x%.0s' $(seq 1 300))\" > \"$HERDR_PLUGIN_STATE_DIR/long.out\" 2>&1"]
[[actions]]
id = "badtone"
command = ["sh", "-c", "herdr agent view-set \"$(cat \"$HERDR_PLUGIN_STATE_DIR/target\")\" --text a --tone loud > \"$HERDR_PLUGIN_STATE_DIR/bad.out\" 2>&1; true"]
[[actions]]
id = "clear"
command = ["sh", "-c", "herdr agent view-clear > \"$HERDR_PLUGIN_STATE_DIR/clear.out\" 2>&1"]
"#,
    );
    s_.json(&["plugin", "link", src.to_str().unwrap()]);
    s_.json(&["plugin", "trust", "acme.view", "--legacy"]);

    // A self-reported agent stands in for a harness run. Self-reports lapse after a few
    // seconds, so each step reports it again and hands the plugin the current handle.
    let created = s_.herdr(&["workspace", "create", "--cwd", "/tmp"]);
    let pane = created["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let state = s_.path("state/plugins/state/acme.view");
    std::fs::create_dir_all(&state).unwrap();
    let agent = || -> String {
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
        let target = agents["agents"][0]["agent_id"]
            .as_str()
            .unwrap()
            .to_string();
        write(&state.join("target"), &target);
        target
    };
    let target = agent();

    let views = |s_: &Session| -> Vec<Value> {
        s_.api("compat.ui.state", json!({}))["agent_views"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    };
    // Only a plugin invocation may set a view: the user-level shim is refused.
    let e = s_.herdr_fail_text(&["agent", "view-set", &target, "--text", "hi"]);
    assert!(e.contains("permission_denied"), "{e}");
    assert!(views(&s_).is_empty());

    // A self-reported agent can lapse while an action runs (the pane has no real agent
    // process), so each step re-reports it and retries when the run was gone meanwhile.
    let attempt = |what: &str, f: &dyn Fn() -> bool| {
        for _ in 0..6 {
            agent();
            if f() {
                return;
            }
        }
        panic!("{what}: the agent kept lapsing or the step failed");
    };
    let act = |name: &str| {
        s_.run("acme.view", name);
        s_.settle("acme.view");
    };
    attempt("set", &|| {
        act("set");
        views(&s_).len() == 1
    });
    let v = views(&s_);
    assert_eq!(v[0]["plugin_id"], "acme.view");
    assert_eq!(v[0]["text"], "building");
    assert_eq!(v[0]["detail"], "step 2 of 5");
    assert_eq!(v[0]["tone"], "warn");
    assert!(!s_.events("plugin.agent_view_changed").is_empty());

    // Size limits and validation.
    attempt("long", &|| {
        act("long");
        views(&s_)
            .first()
            .is_some_and(|v| v["text"].as_str().unwrap().starts_with("xxxx"))
    });
    assert_eq!(
        views(&s_)[0]["text"].as_str().unwrap().chars().count(),
        80,
        "text is cut to 80"
    );
    attempt("badtone", &|| {
        act("badtone");
        read(&state.join("bad.out")).contains("invalid_params")
    });

    // Clearing, and clearing on disable.
    act("clear");
    assert!(views(&s_).is_empty(), "{}", read(&state.join("clear.out")));
    attempt("set again", &|| {
        act("set");
        views(&s_).len() == 1
    });
    s_.json(&["plugin", "disable", "acme.view"]);
    assert!(views(&s_).is_empty(), "disable clears the plugin's views");
    s_.json(&["plugin", "enable", "acme.view"]);
    assert!(
        views(&s_).is_empty(),
        "re-enabling does not bring them back"
    );
    // Untrusting also drops a view set before.
    attempt("set before untrust", &|| {
        act("set");
        views(&s_).len() == 1
    });
    s_.json(&["plugin", "untrust", "acme.view"]);
    assert!(views(&s_).is_empty());
}

impl Session {
    fn herdr_fail_text(&self, args: &[&str]) -> String {
        let mut a = vec!["compat", "herdr"];
        a.extend(args);
        self.fail(&a).1
    }
}

// ---- restricted (sandboxed) legacy mode ----------------------------------------------------

fn write_sandbox_plugin(dir: &Path) {
    write(
        &dir.join("herdr-plugin.toml"),
        r#"id = "acme.sbx"
[[actions]]
id = "probe"
command = ["sh", "bin/probe.sh"]
[[panes]]
id = "main"
placement = "split"
command = ["sh", "-c", "true"]
"#,
    );
    write(
        &dir.join("bin/probe.sh"),
        r#"out="$HERDR_PLUGIN_STATE_DIR"
echo ok > "$out/state_write" 2>/dev/null; echo "state_write=$?" >> "$out/r"
echo x > "$HERDR_PLUGIN_ROOT/planted" 2>/dev/null; echo "root_write=$?" >> "$out/r"
cat "$HERDR_PLUGIN_ROOT/herdr-plugin.toml" > /dev/null 2>&1; echo "root_read=$?" >> "$out/r"
ls "$HERDR_PLUGIN_STATE_DIR/../acme.other" > /dev/null 2>&1; echo "other_state=$?" >> "$out/r"
echo "token=${GITHUB_TOKEN:-unset}" >> "$out/r"
echo "isolation=$VIBEKE_ISOLATION" >> "$out/r"
/usr/bin/nc -z -w 2 127.0.0.1 "$(cat "$out/port")" > /dev/null 2>&1; echo "net=$?" >> "$out/r"
herdr workspace list > "$out/ws.json" 2> "$out/ws.err"; echo "herdr=$?" >> "$out/r"
"#,
    );
}

fn field(text: &str, k: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{k}=")))
        .unwrap_or("missing")
        .to_string()
}

#[cfg(target_os = "macos")]
#[test]
fn a_sandboxed_plugin_runs_restricted() {
    if vk_sandbox::plugin::probe().is_err() {
        eprintln!("skipped: no working sandbox here (nested?)");
        return;
    }
    let s_ = Session::new("[plugins.\"acme.sbx\"]\nisolate = \"sandbox\"\n");
    let src = s_.path("src/sbx");
    write_sandbox_plugin(&src);
    s_.json(&["plugin", "link", src.to_str().unwrap()]);
    s_.json(&["plugin", "trust", "acme.sbx", "--legacy"]);
    let state = s_.path("state/plugins/state/acme.sbx");
    std::fs::create_dir_all(s_.path("state/plugins/state/acme.other")).unwrap();
    // Something to connect to: reachable from the host, not from the sandbox.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::thread::spawn({
        let l = l.try_clone().unwrap();
        move || while l.accept().is_ok() {}
    });
    write(
        &state.join("port"),
        &l.local_addr().unwrap().port().to_string(),
    );

    let p = s_.plugin("acme.sbx");
    assert_eq!(p["isolate"], "sandbox");
    assert_eq!(p["sandbox_available"], true, "{p}");
    assert_eq!(p["network"], false);
    let rec = s_.run("acme.sbx", "probe");
    assert_eq!(rec["isolation"], "sandbox");
    let logs = s_.settle("acme.sbx");
    assert_eq!(logs[0]["status"], "succeeded", "{}", logs[0]);
    let r = read(&state.join("r"));
    assert_eq!(field(&r, "state_write"), "0", "own state is writable\n{r}");
    assert_ne!(field(&r, "root_write"), "0", "plugin dir is read-only\n{r}");
    assert!(!src.join("planted").exists());
    assert_eq!(field(&r, "root_read"), "0", "{r}");
    assert_ne!(
        field(&r, "other_state"),
        "0",
        "other plugins' state is hidden\n{r}"
    );
    assert_eq!(
        field(&r, "token"),
        "unset",
        "the environment is scrubbed\n{r}"
    );
    assert_eq!(field(&r, "isolation"), "sandbox");
    assert_ne!(field(&r, "net"), "0", "network is off\n{r}");
    assert_eq!(
        field(&r, "herdr"),
        "0",
        "the plugin still reaches its own broker\n{r}\n{}",
        read(&state.join("ws.err"))
    );
    assert!(read(&state.join("ws.json")).contains("workspaces"));

    // Plugin panes are not offered in restricted mode.
    let e = s_.api(
        "compat.herdr.call",
        json!({"method": "plugin.pane.open", "params": {"plugin_id": "acme.sbx", "pane": "main"}}),
    );
    assert_eq!(e["error"]["code"], "unsupported", "{e}");

    // Network granted by config: the sandbox opens outbound connections.
    std::fs::write(
        s_.path("config.toml"),
        "[plugins.\"acme.sbx\"]\nisolate = \"sandbox\"\nnetwork = true\n",
    )
    .unwrap();
    std::fs::remove_file(state.join("r")).unwrap();
    s_.run("acme.sbx", "probe");
    s_.settle("acme.sbx");
    let r = read(&state.join("r"));
    assert_eq!(field(&r, "net"), "0", "network = true is honoured\n{r}");
    assert_eq!(s_.plugin("acme.sbx")["network"], true);
}

#[test]
fn restricted_mode_refuses_where_no_sandbox_works() {
    let s_ = Session::with_env(
        "[plugins.\"acme.sbx\"]\nisolate = \"sandbox\"\n",
        &[
            ("VIBEKE_TEST_HOOKS", "1"),
            ("VIBEKE_TEST_PLUGIN_SANDBOX_UNAVAILABLE", "1"),
        ],
    );
    let src = s_.path("src/sbx");
    write_sandbox_plugin(&src);
    s_.json(&["plugin", "link", src.to_str().unwrap()]);
    s_.json(&["plugin", "trust", "acme.sbx", "--legacy"]);
    let state = s_.path("state/plugins/state/acme.sbx");
    let p = s_.plugin("acme.sbx");
    assert_eq!(p["sandbox_available"], false, "{p}");
    assert!(
        p["sandbox_error"]
            .as_str()
            .unwrap()
            .contains("not available")
    );
    let rec = s_.run("acme.sbx", "probe");
    assert_eq!(rec["status"], "failed", "{rec}");
    let err = rec["stderr"].as_str().unwrap();
    assert!(
        err.contains("restricted (sandboxed) plugin mode is not available here"),
        "{err}"
    );
    assert!(
        err.contains("isolate = \"host\""),
        "the fix is named: {err}"
    );
    wait_for("no host fallback", 500, || true);
    assert!(
        !state.join("r").exists(),
        "the plugin must not run on the host instead"
    );
}
