//! Batch 2A API surface against a real server in an isolated VIBEKE_* session: `session.*`,
//! `server.restart`, `config.get|set|validate|reload` (and the file watcher), `blob.get|stat`,
//! `pane.move|scroll|screenshot`, `task.park|resume`, `task.create`/`worktree.remove` dry runs,
//! pane request budgets and spawn depth limits, read-only attach, and the CLI verbs
//! `events tail`, `api call`, `completion` and `--dry-run`.

mod support;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};
use support::{Session, alive};
use vk_proto::render::{AckStatus, ClientFrame, ServerFrame};
use vk_server::api_schema::{validate_event, validate_params, validate_result};

const VK: &str = env!("CARGO_BIN_EXE_vibeke");

fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn kind(e: &Value) -> &str {
    e["data"]["kind"].as_str().unwrap_or("")
}

/// A raw connection whose successful calls (params and result) and, at the end of the test,
/// every event of the session are validated against the schema registry.
struct Rpc(support::Rpc);

impl Rpc {
    fn connect(path: &Path) -> Rpc {
        Rpc(support::Rpc::connect(path))
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let r = self.0.call(method, params.clone());
        if let Ok(v) = &r {
            let mut probs = validate_params(method, &params);
            probs.extend(validate_result(method, v));
            assert!(
                probs.is_empty(),
                "{method}: {probs:?}\nparams {params}\nresult {v}"
            );
        }
        r
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let mut after = 0;
        loop {
            let Ok(v) = self
                .0
                .call("events.read", json!({"after": after, "limit": 500}))
            else {
                return;
            };
            let evs = v["events"].as_array().cloned().unwrap_or_default();
            for e in &evs {
                let probs = validate_event(e);
                assert!(probs.is_empty(), "event {e}: {probs:?}");
                after = e["seq"].as_i64().unwrap_or(after);
            }
            if evs.len() < 500 {
                return;
            }
        }
    }
}

fn rpc(s: &Session) -> Rpc {
    // Any CLI call starts the server.
    s.json(&["server", "status"]);
    Rpc::connect(&s.socket())
}

fn events_of(r: &mut Rpc, ty: &str) -> Vec<Value> {
    r.call(
        "events.read",
        json!({"after": 0, "types": [ty], "limit": 500}),
    )
    .unwrap()["events"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// Stops a side session even when the test fails.
struct SideSession<'a>(&'a Session, &'static str);
impl Drop for SideSession<'_> {
    fn drop(&mut self) {
        let _ = self
            .0
            .cmd(&["--session", self.1, "server", "stop", "--kill-panes"])
            .output();
    }
}

#[test]
fn session_methods_and_cli() {
    let s = Session::new();
    let mut r = rpc(&s);
    let list = r.call("session.list", json!({})).unwrap();
    let cur = list["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == "default")
        .cloned()
        .expect("default listed");
    assert_eq!(cur["running"], true);
    assert_eq!(cur["current"], true);
    assert!(cur["pid"].as_u64().is_some());

    let bad = r
        .call("session.create", json!({"name": "../x"}))
        .unwrap_err();
    assert_eq!(kind(&bad), "invalid_params");
    let _guard = SideSession(&s, "side");
    let created = s.json(&["session", "new", "side"]);
    assert_eq!(created["session"]["running"], true, "{created}");
    let taken = r
        .call("session.create", json!({"name": "side"}))
        .unwrap_err();
    assert_eq!(kind(&taken), "conflict");
    let running = r
        .call(
            "session.rename",
            json!({"name": "side", "new_name": "side2"}),
        )
        .unwrap_err();
    assert!(
        running["message"]
            .as_str()
            .unwrap()
            .contains("session_running")
    );
    let names = |r: &mut Rpc| -> Vec<(String, bool)> {
        r.call("session.list", json!({})).unwrap()["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| {
                (
                    x["name"].as_str().unwrap().to_string(),
                    x["running"] == true,
                )
            })
            .collect()
    };
    assert!(names(&mut r).contains(&("side".into(), true)));
    let stopped = s.json(&["session", "stop", "side"]);
    assert_eq!(stopped["stopped"], true, "{stopped}");
    assert!(names(&mut r).contains(&("side".into(), false)));
    let renamed = s.json(&["session", "rename", "side", "side2"]);
    assert_eq!(renamed["previous_name"], "side");
    let after = names(&mut r);
    assert!(after.contains(&("side2".into(), false)), "{after:?}");
    assert!(!after.iter().any(|(n, _)| n == "side"));
    let gone = r.call("session.stop", json!({"name": "nope"})).unwrap_err();
    assert_eq!(kind(&gone), "not_found");
}

#[test]
fn server_restart_keeps_panes() {
    let s = Session::new();
    let pane = s.workspace("sleep 1000");
    let child = s.pane(&pane)["child_pid"].as_i64().unwrap();
    let before = s.json(&["server", "status"]);
    let out = s.cmd(&["server", "restart"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // `server.restart` answers, then execs: a status call right after can still reach the old
    // image, so wait until the new one (same pid, fresh uptime) answers.
    let mut after = Value::Null;
    let before_up = before["uptime_ms"].as_u64().unwrap();
    wait_until("restarted image answers", Duration::from_secs(10), || {
        let Ok(st) = s.cmd(&["server", "status"]).output() else {
            return false;
        };
        let v: Value = serde_json::from_slice(&st.stdout).unwrap_or(Value::Null);
        let fresh = v["uptime_ms"].as_u64().is_some_and(|u| u < before_up);
        after = v;
        fresh
    });
    assert_eq!(after["pid"], before["pid"], "exec keeps the pid");
    assert!(alive(child), "the pane process survives the restart");
    wait_until("pane recovered", Duration::from_secs(10), || {
        s.pane(&pane)["id"] == pane.as_str()
    });
    let mut r = Rpc::connect(&s.socket());
    let bad = r
        .call("server.restart", json!({"binary": "relative/vibeke"}))
        .unwrap_err();
    assert_eq!(kind(&bad), "invalid_params");
    assert!(!events_of(&mut r, "session.server_restarted").is_empty());
}

#[test]
fn config_get_set_validate_reload_and_watch() {
    let s = Session::new();
    let cfg = s.dir.path().join("config.toml");
    std::fs::write(
        &cfg,
        "# my settings\n[terminal]\n# keep a lot\nscrollback_lines = 5000 # mine\n",
    )
    .unwrap();
    let mut r = rpc(&s);
    let g = r
        .call("config.get", json!({"key": "terminal.scrollback_lines"}))
        .unwrap();
    assert_eq!(g["value"], 5000);
    assert_eq!(g["source"], "user");
    let g = s.json(&["config", "get", "theme.name"]);
    assert_eq!(g["source"], "default");
    assert!(
        r.call("config.get", json!({"key": "nope.nope"}))
            .unwrap_err()["data"]["kind"]
            == "not_found"
    );

    // Runtime override: applied, file untouched.
    let set = s.json(&["config", "set", "theme.name", "nord"]);
    assert_eq!(set["persisted"], false);
    assert_eq!(set["changed"], json!(["theme.name"]));
    let g = r.call("config.get", json!({"key": "theme.name"})).unwrap();
    assert_eq!(
        (g["value"].as_str(), g["source"].as_str()),
        (Some("nord"), Some("runtime"))
    );
    assert!(!std::fs::read_to_string(&cfg).unwrap().contains("nord"));

    // Persisted: written atomically with comments kept.
    let set = r
        .call(
            "config.set",
            json!({"key": "terminal.scrollback_lines", "value": 7000, "persist": true}),
        )
        .unwrap();
    assert_eq!(set["persisted"], true);
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(
        text.contains("# my settings") && text.contains("# keep a lot"),
        "{text}"
    );
    assert!(text.contains("scrollback_lines = 7000 # mine"), "{text}");
    let g = r
        .call("config.get", json!({"key": "terminal.scrollback_lines"}))
        .unwrap();
    assert_eq!(
        (g["value"].as_i64(), g["source"].as_str()),
        (Some(7000), Some("user"))
    );

    // Refusals: wrong type, unknown key.
    let e = r
        .call(
            "config.set",
            json!({"key": "terminal.scrollback_lines", "value": "lots"}),
        )
        .unwrap_err();
    assert_eq!(kind(&e), "invalid_params", "{e}");
    let e = r
        .call(
            "config.set",
            json!({"key": "terminal.no_such_key", "value": 1}),
        )
        .unwrap_err();
    assert_eq!(kind(&e), "invalid_params", "{e}");
    // Reset the override.
    r.call("config.set", json!({"key": "theme.name", "value": null}))
        .unwrap();
    let g = r.call("config.get", json!({"key": "theme.name"})).unwrap();
    assert_eq!(g["source"], "runtime");
    assert_eq!(g["value"], "catppuccin");

    // Validate a broken file: located errors.
    let broken = s.dir.path().join("broken.toml");
    std::fs::write(&broken, "[terminal]\nscrollback_lines = \n").unwrap();
    let v = r.call("config.validate", json!({"path": broken})).unwrap();
    assert_eq!(v["valid"], false);
    assert_eq!(v["errors"][0]["line"], 2, "{v}");
    let v = s.json(&["api", "call", "config.validate", "{}"]);
    assert_eq!(v["valid"], true, "{v}");

    // The watcher applies an external edit and emits session.config_reloaded.
    let before = events_of(&mut r, "session.config_reloaded").len();
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, text.replace("7000", "9000")).unwrap();
    wait_until("watch reload", Duration::from_secs(10), || {
        events_of(&mut r, "session.config_reloaded")
            .iter()
            .skip(before)
            .any(|e| e["data"]["source"] == "watch")
    });
    let g = r
        .call("config.get", json!({"key": "terminal.scrollback_lines"}))
        .unwrap();
    assert_eq!(g["value"], 9000);
    // A broken edit is rejected and the applied config stays.
    std::fs::write(&cfg, "[terminal\n").unwrap();
    wait_until("rejection", Duration::from_secs(10), || {
        !events_of(&mut r, "session.config_rejected").is_empty()
    });
    let rl = s.json(&["config", "reload"]);
    assert_eq!(rl["changed"], json!([]));
    assert_eq!(rl["errors"][0]["line"], 1, "{rl}");
    let e = s.json(&["server", "reload-config"]);
    assert!(!e["errors"].as_array().unwrap().is_empty());
}

#[test]
fn blob_get_and_stat() {
    use base64::Engine;
    let s = Session::new();
    let mut r = rpc(&s);
    let data = b"hello blob store";
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    let put = r
        .call(
            "blob.put",
            json!({"mime": "text/plain", "data_b64": b64, "name": "note.txt"}),
        )
        .unwrap();
    let hash = put["hash"].as_str().unwrap().to_string();
    let st = r.call("blob.stat", json!({"hash": hash})).unwrap();
    assert_eq!(st["size"], data.len());
    assert_eq!(st["mime"], "text/plain");
    assert_eq!(st["refs"], 1);
    let g = r.call("blob.get", json!({"hash": hash})).unwrap();
    assert_eq!(g["data_b64"], b64);
    let part = r
        .call(
            "blob.get",
            json!({"hash": hash, "range": {"offset": 6, "length": 4}}),
        )
        .unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(part["data_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(bytes, b"blob");
    let out = s.dir.path().join("got.txt");
    s.json(&["blob", "get", &hash, "--out", out.to_str().unwrap()]);
    assert_eq!(std::fs::read(&out).unwrap(), data);
    let cli = s.json(&[
        "api",
        "call",
        "blob.stat",
        &json!({"hash": hash}).to_string(),
    ]);
    assert_eq!(cli["hash"], hash.as_str());
    let missing = r
        .call("blob.stat", json!({"hash": "0".repeat(64)}))
        .unwrap_err();
    assert_eq!(kind(&missing), "not_found");
    let bad = r.call("blob.get", json!({"hash": "xyz"})).unwrap_err();
    assert_eq!(kind(&bad), "invalid_params");
}

#[test]
fn pane_screenshot_scroll_and_move() {
    let s = Session::new();
    let script = s.dir.path().join("colors.sh");
    std::fs::write(
        &script,
        "printf '\\033[31mRED\\033[0m plain\\n'; seq 1 300; sleep 1000\n",
    )
    .unwrap();
    let pane = s.workspace(&format!("sh {}", script.display()));
    wait_until("seq output", Duration::from_secs(10), || {
        s.read(&pane, "recent", "400")
            .lines()
            .any(|l| l.trim() == "300")
    });
    let mut r = rpc(&s);
    let shot = r
        .call(
            "pane.screenshot",
            json!({"pane": pane, "format": "ansi", "source": "recent", "lines": 400, "inline": true}),
        )
        .unwrap();
    let data = shot["data"].as_str().unwrap();
    assert!(data.contains("\x1b[31mRED\x1b[0m plain"), "{data:?}");
    assert_eq!(shot["blob"]["mime"], "text/x-ansi");
    let hash = shot["blob"]["hash"].as_str().unwrap();
    let st = r.call("blob.stat", json!({"hash": hash})).unwrap();
    assert_eq!(st["mime"], "text/x-ansi");
    let html = r
        .call(
            "pane.screenshot",
            json!({"pane": pane, "format": "html", "inline": true}),
        )
        .unwrap();
    assert!(
        html["data"]
            .as_str()
            .unwrap()
            .starts_with("<!doctype html>")
    );
    let out = s.dir.path().join("shot.txt");
    s.json(&[
        "pane",
        "screenshot",
        &pane,
        "--format",
        "text",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert!(std::fs::read_to_string(&out).unwrap().contains("300"));
    // PNG and SVG renderings of the grid (v1_remainder.rs checks their content).
    let png = r
        .call("pane.screenshot", json!({"pane": pane, "format": "png"}))
        .unwrap();
    assert_eq!(png["blob"]["mime"], "image/png");
    assert!(png["width"].as_u64().unwrap() > 0);

    // Scroll requests: offsets clamp to the scrollback and are published as events.
    let top = r
        .call("pane.scroll", json!({"pane": pane, "to": "top"}))
        .unwrap();
    let total = top["scroll"]["total"].as_u64().unwrap();
    assert!(total > 0, "{top}");
    assert_eq!(top["scroll"]["offset"], total);
    let down = r
        .call("pane.scroll", json!({"pane": pane, "delta": -5}))
        .unwrap();
    assert_eq!(down["scroll"]["offset"], total - 5);
    let bottom = s.json(&["pane", "scroll", &pane, "bottom"]);
    assert_eq!(bottom["scroll"]["at_bottom"], true);
    let ev = events_of(&mut r, "pane.scroll_requested");
    assert_eq!(ev.len(), 3);
    assert!(r.call("pane.scroll", json!({"pane": pane})).is_err());

    // Move: into another tab, into another workspace, into a new tab.
    let ws = s.pane(&pane)["workspace"].as_str().unwrap().to_string();
    let split = s.split(&pane, "sleep 1000");
    let tab2 = r
        .call(
            "tab.create",
            json!({"workspace": ws, "command": ["sleep", "1000"]}),
        )
        .unwrap();
    let tab2_id = tab2["tab"]["id"].as_str().unwrap().to_string();
    let moved = r
        .call(
            "pane.move",
            json!({"pane": split, "to": {"tab": tab2_id}, "direction": "down"}),
        )
        .unwrap();
    assert_eq!(moved["pane"]["tab"], tab2_id.as_str());
    assert_eq!(moved["source_tab_closed"], false);
    let same = r
        .call("pane.move", json!({"pane": split, "to": {"tab": tab2_id}}))
        .unwrap_err();
    assert_eq!(kind(&same), "conflict");
    let other = s.json(&[
        "workspace",
        "create",
        "--cwd",
        "/tmp",
        "--command",
        "sleep 1000",
    ]);
    let ws2 = other["workspace"]["id"].as_str().unwrap().to_string();
    let ws2_handle = other["workspace"]["handle"].as_str().unwrap().to_string();
    let old_handle = s.pane(&split)["handle"].as_str().unwrap().to_string();
    let moved = s.json(&["pane", "move", &split, "--to-workspace", &ws2]);
    assert_eq!(moved["previous_pane_handle"], old_handle.as_str());
    assert_eq!(moved["pane"]["workspace"], ws2.as_str());
    assert!(
        moved["pane"]["handle"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{ws2_handle}:")),
        "{moved}"
    );
    let moved = r
        .call(
            "pane.move",
            json!({"pane": split, "to": {"new_tab_in": ws2}}),
        )
        .unwrap();
    assert_eq!(
        moved["tab"]["layout"]["Leaf"]["pane"],
        split.as_str(),
        "{moved}"
    );
    assert!(!events_of(&mut r, "pane.moved").is_empty());
    // The process kept running throughout.
    let p = s.pane(&split);
    assert!(alive(p["child_pid"].as_i64().unwrap()));
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

#[test]
fn task_park_resume_and_dry_runs() {
    let s = Session::new();
    let repo = s.dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let repo_s = repo.to_str().unwrap();
    let mut r = rpc(&s);

    // Dry runs touch nothing.
    let plan = s.json(&["task", "new", "fix login", "--repo", repo_s, "--dry-run"]);
    assert_eq!(plan["dry_run"], true, "{plan}");
    assert_eq!(plan["plan"]["checkout"], "worktree");
    assert!(
        plan["plan"]["branch"]
            .as_str()
            .unwrap()
            .ends_with("fix-login"),
        "{plan}"
    );
    assert!(!Path::new(plan["plan"]["path"].as_str().unwrap()).exists());
    assert_eq!(r.call("task.list", json!({})).unwrap()["tasks"], json!([]));
    let wr = s.json(&["worktree", "remove", repo_s, "--dry-run"]);
    assert_eq!(wr["dry_run"], true);
    assert!(repo.join("a.txt").exists());

    let t = r
        .call(
            "task.create",
            json!({"title": "park me", "repo": repo_s, "isolation": "none", "setup": false}),
        )
        .unwrap();
    let task = t["task"]["id"].as_str().unwrap().to_string();
    let pane = t["panes"][0]["id"].as_str().unwrap().to_string();
    // A pretend agent: a foreground process in the task's shell, bound as a run.
    std::thread::sleep(Duration::from_millis(500));
    r.call("pane.run", json!({"pane": pane, "command": "sleep 1001"}))
        .unwrap();
    let fg_pid = || -> Option<i64> {
        let out = std::process::Command::new("pgrep")
            .args(["-f", "sleep 1001"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
    };
    wait_until("agent process", Duration::from_secs(10), || {
        fg_pid().is_some()
    });
    let sleeper = fg_pid().unwrap();
    r.call(
        "agent.report",
        json!({"pane": pane, "state": "working", "harness": "claude"}),
    )
    .unwrap();
    let parked = s.json(&["task", "park", &task]);
    assert_eq!(parked["task"]["status"], "parked", "{parked}");
    assert_eq!(parked["stopped"].as_array().unwrap().len(), 1, "{parked}");
    assert_eq!(parked["stopped"][0]["pane_closed"], false);
    wait_until("agent stopped", Duration::from_secs(10), || !alive(sleeper));
    assert_eq!(s.pane(&pane)["id"], pane.as_str(), "the shell pane stays");
    let again = r.call("task.park", json!({"task": task})).unwrap_err();
    assert_eq!(kind(&again), "conflict");
    let resumed = s.json(&["task", "resume", &task]);
    assert_eq!(resumed["task"]["status"], "active");
    assert_eq!(
        resumed["skipped"][0]["reason"], "no resume handle",
        "{resumed}"
    );
    assert!(!events_of(&mut r, "task.parked").is_empty());
    assert!(!events_of(&mut r, "task.resumed").is_empty());
    let not_parked = r.call("task.resume", json!({"task": task})).unwrap_err();
    assert_eq!(kind(&not_parked), "conflict");
}

#[test]
fn pane_budgets_and_spawn_depth() {
    let s = Session::new();
    std::fs::write(
        s.dir.path().join("config.toml"),
        "[security.limits]\nburst = 3\nrate = 0.1\nmax_spawn_depth = 1\n",
    )
    .unwrap();
    let d = s.dir.path();
    // Request budget: the second CLI call from the pane (hello + call each) runs dry.
    let budget = d.join("budget.sh");
    std::fs::write(
        &budget,
        format!(
            "for i in 1 2 3; do {VK} --json pane list >/dev/null 2>>{d}/budget.err && echo ok >> {d}/budget.out; done; echo done >> {d}/budget.out; sleep 1000\n",
            d = d.display()
        ),
    )
    .unwrap();
    s.workspace(&format!("sh {}", budget.display()));
    wait_until("budget script", Duration::from_secs(20), || {
        std::fs::read_to_string(d.join("budget.out")).is_ok_and(|t| t.contains("done"))
    });
    let out = std::fs::read_to_string(d.join("budget.out")).unwrap();
    assert_eq!(out.matches("ok").count(), 1, "{out}");
    let err = std::fs::read_to_string(d.join("budget.err")).unwrap();
    assert!(err.contains("rate_limited"), "{err}");
    let mut r = Rpc::connect(&s.socket());
    let ev = events_of(&mut r, "security.rate_limited");
    assert!(
        ev.iter().any(|e| e["data"]["limit"] == "requests"),
        "{ev:?}"
    );
    // Full scope is never limited.
    for _ in 0..10 {
        r.call("pane.list", json!({})).unwrap();
    }

    // Depth: a pane split by an agent is depth 1; splitting again from it is refused.
    let inner = d.join("inner.sh");
    std::fs::write(
        &inner,
        format!(
            "{VK} --json pane split --current --direction down --command 'sleep 1000' >/dev/null 2>{d}/depth.err; echo $? > {d}/depth.code; sleep 1000\n",
            d = d.display()
        ),
    )
    .unwrap();
    let outer = d.join("outer.sh");
    std::fs::write(
        &outer,
        format!(
            "{VK} --json pane split --current --direction right --command 'sh {}' >/dev/null 2>{d}/outer.err; echo $? > {d}/outer.code; sleep 1000\n",
            inner.display(),
            d = d.display()
        ),
    )
    .unwrap();
    s.workspace(&format!("sh {}", outer.display()));
    wait_until("depth script", Duration::from_secs(20), || {
        std::fs::read_to_string(d.join("depth.code")).is_ok()
    });
    assert_eq!(
        std::fs::read_to_string(d.join("outer.code"))
            .unwrap()
            .trim(),
        "0",
        "{}",
        std::fs::read_to_string(d.join("outer.err")).unwrap_or_default()
    );
    let depth = std::fs::read_to_string(d.join("depth.err")).unwrap();
    assert!(depth.contains("spawn_depth_exceeded"), "{depth}");
}

#[test]
fn readonly_attach_refuses_input_and_mutations() {
    let s = Session::new();
    let pane = s.workspace("sleep 1000");
    let sock = UnixStream::connect(s.socket()).unwrap();
    let mut w = sock.try_clone().unwrap();
    let mut rd = BufReader::new(sock);
    let hello = json!({"jsonrpc":"2.0","id":1,"method":"client.hello","params":{"client":"t","version":"0","api":"vibeke/1","kind":"tui","readonly":true}});
    let attach = json!({"jsonrpc":"2.0","id":2,"method":"render.attach","params":{"client_id":"ro-test","protocol": vk_proto::render::PROTOCOL,"caps":{"max_fps":30}}});
    w.write_all(format!("{hello}\n").as_bytes()).unwrap();
    let mut line = String::new();
    rd.read_line(&mut line).unwrap();
    assert!(line.contains("\"id\":1"), "{line}");
    w.write_all(format!("{attach}\n").as_bytes()).unwrap();
    line.clear();
    rd.read_line(&mut line).unwrap();
    assert!(!line.contains("error"), "{line}");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(f) = vk_proto::frame::read_frame::<_, ServerFrame>(&mut rd) {
            if tx.send(f).is_err() {
                break;
            }
        }
    });
    let send = |w: &mut UnixStream, f: &ClientFrame| {
        vk_proto::frame::write_frame(w, f).unwrap();
        w.flush().unwrap();
    };
    send(
        &mut w,
        &ClientFrame::RawInput {
            input_id: 42,
            pane: pane.clone(),
            bytes: b"echo hi\r".to_vec(),
        },
    );
    send(
        &mut w,
        &ClientFrame::Command {
            req: 7,
            json: json!({"jsonrpc":"2.0","id":7,"method":"pane.close","params":{"pane": pane}})
                .to_string(),
        },
    );
    send(
        &mut w,
        &ClientFrame::Command {
            req: 8,
            json: json!({"jsonrpc":"2.0","id":8,"method":"pane.list","params":{}}).to_string(),
        },
    );
    let (mut ack, mut refused, mut listed) = (None, None, None);
    let t0 = Instant::now();
    while (ack.is_none() || refused.is_none() || listed.is_none())
        && t0.elapsed() < Duration::from_secs(10)
    {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ServerFrame::InputAck {
                input_id: 42,
                status,
            }) => ack = Some(status),
            Ok(ServerFrame::CommandResult { req: 7, json }) => refused = Some(json),
            Ok(ServerFrame::CommandResult { req: 8, json }) => listed = Some(json),
            _ => {}
        }
    }
    assert_eq!(ack, Some(AckStatus::Rejected));
    assert!(refused.unwrap().contains("read-only"));
    assert!(listed.unwrap().contains("\"panes\""));
    assert_eq!(
        s.pane(&pane)["id"],
        pane.as_str(),
        "the pane was not closed"
    );
}

#[test]
fn cli_events_tail_and_completion() {
    let s = Session::new();
    s.json(&[
        "workspace",
        "create",
        "--cwd",
        "/tmp",
        "--command",
        "sleep 1000",
    ]);
    let out = s
        .cmd(&["events", "tail", "--types", "workspace.*", "--lines", "5"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let first: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(first["type"], "workspace.created");

    // --follow: a live event arrives as one JSON line.
    let mut child = s
        .cmd(&[
            "events",
            "tail",
            "--types",
            "workspace.created",
            "--lines",
            "0",
            "--follow",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    // Create workspaces until one shows up (the subscription may start after the first).
    let t0 = Instant::now();
    let line = loop {
        s.json(&[
            "workspace",
            "create",
            "--cwd",
            "/tmp",
            "--name",
            "followed",
            "--command",
            "sleep 1000",
        ]);
        if let Ok(l) = rx.recv_timeout(Duration::from_millis(500)) {
            break l;
        }
        assert!(t0.elapsed() < Duration::from_secs(15), "no followed event");
    };
    let _ = child.kill();
    let _ = child.wait();
    let ev: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(ev["type"], "workspace.created");

    for sh in ["bash", "zsh", "fish", "nu", "powershell"] {
        let out = std::process::Command::new(VK)
            .args(["completion", sh])
            .output()
            .unwrap();
        assert!(out.status.success(), "{sh}");
        let script = String::from_utf8_lossy(&out.stdout);
        assert!(
            script.contains("session") && script.contains("park"),
            "{sh}"
        );
    }
    let bad = std::process::Command::new(VK)
        .args(["completion", "tcsh"])
        .output()
        .unwrap();
    assert_eq!(bad.status.code(), Some(2));
    if Path::new("/bin/bash").exists() {
        let st = std::process::Command::new("/bin/bash")
            .args([
                "-n",
                "-c",
                &String::from_utf8_lossy(
                    &std::process::Command::new(VK)
                        .args(["completion", "bash"])
                        .output()
                        .unwrap()
                        .stdout,
                ),
            ])
            .status()
            .unwrap();
        assert!(st.success(), "bash completion parses");
    }
}
