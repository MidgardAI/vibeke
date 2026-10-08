//! The v1 server/API/CLI remainder against a real server in an isolated session:
//! `pane.sync_input`, PNG/SVG `pane.screenshot`, `task.adopt|setup_log|ports|ports.re_lease|
//! recreate|forget|archive`, the PR-merged cleanup hint, config layers (`--config-override`,
//! `VIBEKE_CONFIG_OVERRIDE`, trusted repo `.vibeke/config.toml` in `config.get`), `config edit`
//! and `config reset-keys`, `shell-integration`, audit rotation and offline audit records.
//! Every successful call and every event of the session is validated against the schema.

mod support;

use serde_json::{Value, json};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};
use support::Session;
use vk_server::api_schema::{validate_event, validate_params, validate_result};

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

struct Rpc(support::Rpc);

impl Rpc {
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
    s.json(&["server", "status"]);
    Rpc(support::Rpc::connect(&s.socket()))
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

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn repo(s: &Session) -> std::path::PathBuf {
    let repo = s.dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo.canonicalize().unwrap()
}

#[test]
fn sync_input_mirrors_user_input_and_excludes_agents() {
    let s = Session::new();
    let a = s.workspace("cat");
    let b = s.split(&a, "cat");
    let c = s.split(&a, "cat");
    let mut r = rpc(&s);
    // c runs an agent: excluded unless included explicitly.
    r.call(
        "agent.report",
        json!({"pane": c, "state": "working", "harness": "claude"}),
    )
    .unwrap();
    let g = s.json(&[
        "pane",
        "sync-input",
        "start",
        "--panes",
        &format!("{a},{b},{c}"),
    ]);
    assert_eq!(g["group"]["panes"], json!([a, b]), "{g}");
    assert_eq!(g["excluded"][0]["reason"], "agent");
    r.call("pane.send_text", json!({"pane": a, "text": "hello-sync\n"}))
        .unwrap();
    s.wait_output(&b, "hello-sync", 5000);
    assert!(
        !s.read(&c, "recent", "50").contains("hello-sync"),
        "agent pane untouched"
    );
    let seg = r.call("status.segments", json!({"pane": a})).unwrap();
    assert_eq!(seg["segments"]["sync_input"]["enabled"], true, "{seg}");
    let st = s.json(&[
        "pane",
        "sync-input",
        "stop",
        "--group",
        g["group_id"].as_str().unwrap(),
    ]);
    assert_eq!(st["stopped"].as_array().unwrap().len(), 1);
    r.call("pane.send_text", json!({"pane": a, "text": "after-stop\n"}))
        .unwrap();
    s.wait_output(&a, "after-stop", 5000);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!s.read(&b, "recent", "50").contains("after-stop"));
    assert_eq!(events_of(&mut r, "pane.sync_input_changed").len(), 2);
}

#[test]
fn pane_screenshot_png_and_svg() {
    let s = Session::new();
    let pane = s.workspace("sh -c \"printf '\\033[31mRED\\033[0m <&>\\n'; sleep 1000\"");
    s.wait_output(&pane, "RED", 5000);
    let mut r = rpc(&s);
    let svg = r
        .call(
            "pane.screenshot",
            json!({"pane": pane, "format": "svg", "inline": true, "include_cursor": true}),
        )
        .unwrap();
    let text = svg["data"].as_str().unwrap();
    assert!(text.starts_with("<?xml") && text.contains("<svg"), "{text}");
    assert!(
        text.contains(">RED</text>") || text.contains(">RED &lt;&amp;&gt;</text>"),
        "{text}"
    );
    assert_eq!(svg["blob"]["mime"], "image/svg+xml");
    let png = r
        .call(
            "pane.screenshot",
            json!({"pane": pane, "format": "png", "inline": true}),
        )
        .unwrap();
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(png["data_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(&bytes[1..4], b"PNG");
    let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    assert_eq!(w as u64, png["width"].as_u64().unwrap());
    let out = s.dir.path().join("shot.png");
    s.json(&[
        "pane",
        "screenshot",
        &pane,
        "--format",
        "png",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(&std::fs::read(&out).unwrap()[1..4], b"PNG");
}

#[test]
fn task_adopt_ports_recreate_forget_archive_and_pr_hint() {
    let s = Session::new();
    let repo = repo(&s);
    // A fake `gh` that reports the branch's PR as merged.
    let gh = s.dir.path().join("gh");
    std::fs::write(
        &gh,
        "#!/bin/sh\ncase \"$1\" in auth) exit 0;; pr) echo '{\"number\":7,\"state\":\"MERGED\",\"isDraft\":false,\"reviewDecision\":null,\"statusCheckRollup\":[],\"url\":\"https://example.invalid/7\"}';; esac\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let st = s
        .cmd(&["server", "status"])
        .env("VIBEKE_TEST_HOOKS", "1")
        .env("VIBEKE_GH_BIN", &gh)
        .output()
        .unwrap();
    assert!(st.status.success());
    let mut r = Rpc(support::Rpc::connect(&s.socket()));

    let wt = s.dir.path().join("repo-feat");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
    );
    let adopted = s.json(&[
        "task",
        "adopt",
        wt.to_str().unwrap(),
        "--title",
        "feat work",
    ]);
    let task = adopted["task"]["id"].as_str().unwrap().to_string();
    assert_eq!(adopted["task"]["branch"], "feat");
    assert_eq!(adopted["task"]["checkout"], "worktree");
    assert_eq!(adopted["created_workspace"], true);
    assert!(wt.join("a.txt").exists(), "nothing moved");
    let again = r.call("task.adopt", json!({"path": wt})).unwrap_err();
    assert_eq!(kind(&again), "conflict");

    // Setup log (none yet), then ports and a re-lease.
    let log = s.json(&["task", "setup-log", &task]);
    assert_eq!(log["exists"], false);
    let ports = s.json(&["task", "ports", &task]);
    let old = ports["lease"].clone();
    assert!(old["start"].as_u64().is_some(), "{ports}");
    let moved = s.json(&["task", "ports", &task, "--re-lease"]);
    assert_ne!(moved["lease"]["start"], old["start"], "{moved}");
    assert_eq!(moved["lease"]["count"], old["count"]);
    assert_eq!(moved["old_lease"]["start"], old["start"]);
    assert_eq!(events_of(&mut r, "task.ports_changed").len(), 1);

    // The PR is merged: one cleanup hint.
    s.json(&["task", "pr", &task, "--refresh"]);
    s.json(&["task", "pr", &task, "--refresh"]);
    let hints = events_of(&mut r, "task.cleanup_suggested");
    assert_eq!(hints.len(), 1, "{hints:?}");
    assert_eq!(hints[0]["data"]["pr"], 7);

    // Removed outside Vibeke: missing, then recreated from the branch.
    std::fs::remove_dir_all(&wt).unwrap();
    s.json(&["task", "reconcile"]);
    let t = r.call("task.get", json!({"task": task})).unwrap();
    assert_eq!(t["task"]["status"], "missing");
    let rec = s.json(&["task", "recreate", &task]);
    assert_eq!(rec["task"]["status"], "active");
    assert!(wt.join("a.txt").exists());
    // Removed again: forgotten this time, with no files touched.
    std::fs::remove_dir_all(&wt).unwrap();
    s.json(&["task", "reconcile"]);
    let f = s.json(&["task", "forget", &task]);
    assert_eq!(f["task"]["status"], "forgotten");
    assert!(r.call("task.get", json!({"task": task})).is_err());
    assert!(git(&repo, &["branch", "--list", "feat"]).contains("feat"));

    // Archive: refused while dirty, then the worktree goes and the branch stays.
    let wt2 = s.dir.path().join("repo-arch");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "arch", wt2.to_str().unwrap()],
    );
    let t2 = r.call("task.adopt", json!({"path": wt2})).unwrap();
    let task2 = t2["task"]["id"].as_str().unwrap().to_string();
    std::fs::write(wt2.join("dirty.txt"), "x").unwrap();
    let dirty = r.call("task.archive", json!({"task": task2})).unwrap_err();
    assert_eq!(kind(&dirty), "conflict");
    assert_eq!(dirty["data"]["details"]["reason"], "dirty_checkout");
    std::fs::remove_file(wt2.join("dirty.txt")).unwrap();
    let arch = s.json(&["task", "archive", &task2]);
    assert_eq!(arch["worktree_removed"], true, "{arch}");
    assert_eq!(arch["branch_kept"], "arch");
    wait_until("worktree removed", Duration::from_secs(20), || {
        !wt2.exists()
    });
    assert!(git(&repo, &["branch", "--list", "arch"]).contains("arch"));
    assert_eq!(events_of(&mut r, "task.archived").len(), 1);
    assert_eq!(events_of(&mut r, "task.adopted").len(), 2);
}

#[test]
fn config_layers_edit_and_reset_keys() {
    let s = Session::new();
    let cfg = s.dir.path().join("config.toml");
    std::fs::write(
        &cfg,
        "[keys]\nprefix = \"ctrl+a\"\n[[keys.command]]\nkey = \"prefix+t\"\ncommand = \"make\"\n",
    )
    .unwrap();
    // A CLI-layer override the server inherits from the command that starts it.
    let st = s
        .cmd(&["server", "status"])
        .env("VIBEKE_CONFIG_OVERRIDE", "ui.animate=false")
        .output()
        .unwrap();
    assert!(st.status.success());
    let mut r = Rpc(support::Rpc::connect(&s.socket()));
    let v = r.call("config.get", json!({"key": "ui.animate"})).unwrap();
    assert_eq!(
        (v["value"].clone(), v["source"].clone()),
        (json!(false), json!("cli")),
        "{v}"
    );
    let v = r.call("config.get", json!({"key": "keys.prefix"})).unwrap();
    assert_eq!(v["source"], "user");
    // A bad override is refused before anything runs.
    let bad = s
        .cmd(&["--config-override", "ui.animate=notabool", "config", "path"])
        .output()
        .unwrap();
    assert_eq!(
        bad.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&bad.stderr)
    );
    let ok = s
        .cmd(&["--config-override", "ui.animate=true", "config", "path"])
        .output()
        .unwrap();
    assert!(ok.status.success());

    // The repo layer: ignored until trusted, then above the user's file.
    let repo = repo(&s);
    std::fs::create_dir_all(repo.join(".vibeke")).unwrap();
    std::fs::write(
        repo.join(".vibeke/config.toml"),
        "[tasks]\nport_block = 30\n",
    )
    .unwrap();
    let rs = repo.to_str().unwrap();
    let v = s.json(&["config", "get", "tasks.port_block", "--repo", rs]);
    assert_eq!(v["source"], "default", "{v}");
    assert_eq!(v["repo"]["applied"], false);
    let t = s
        .cmd(&["trust", rs, "--yes"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(t.status.success(), "{}", String::from_utf8_lossy(&t.stderr));
    let v = s.json(&["config", "get", "tasks.port_block", "--repo", rs]);
    assert_eq!(
        (v["value"].clone(), v["source"].clone()),
        (json!(30), json!("repo")),
        "{v}"
    );
    assert!(
        v["layers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["source"] == "repo" && l["applied"] == true)
    );
    let _ = r
        .call("config.get", json!({"key": "tasks.port_block", "cwd": rs}))
        .unwrap();

    // config edit: an editor that writes an invalid file, then a valid one.
    let editor = s.dir.path().join("ed.sh");
    std::fs::write(
        &editor,
        "#!/bin/sh\nprintf '[ui]\\nanimate = \"nope\"\\n' > \"$1\"\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let e = s
        .cmd(&["config", "edit"])
        .env("VISUAL", &editor)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        e.status.code(),
        Some(1),
        "invalid file: {}",
        String::from_utf8_lossy(&e.stderr)
    );
    std::fs::write(&editor, "#!/bin/sh\nprintf '[keys]\\nprefix = \"ctrl+a\"\\nsplit_right = \"prefix+v\"\\n[[keys.command]]\\nkey = \"prefix+t\"\\ncommand = \"make\"\\n' > \"$1\"\n").unwrap();
    let e = s
        .cmd(&["config", "edit"])
        .env("VISUAL", &editor)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(e.status.success(), "{}", String::from_utf8_lossy(&e.stderr));
    let out: Value = serde_json::from_slice(&e.stdout).unwrap();
    assert_eq!(out["valid"], true);

    // reset-keys: refused without --yes off a terminal; then keys gone, commands kept, backup.
    let no = s
        .cmd(&["config", "reset-keys"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(no.status.code(), Some(2));
    let rk = s.json(&["config", "reset-keys", "--yes"]);
    assert_eq!(
        rk["removed"],
        json!(["keys.prefix", "keys.split_right"]),
        "{rk}"
    );
    let backup = rk["backup"].as_str().unwrap();
    assert!(std::fs::read_to_string(backup).unwrap().contains("ctrl+a"));
    let now = std::fs::read_to_string(&cfg).unwrap();
    assert!(!now.contains("ctrl+a") && now.contains("make"), "{now}");
}

#[test]
fn shell_integration_snippets() {
    let s = Session::new();
    for sh in ["zsh", "bash", "fish"] {
        let out = s.cmd(&["shell-integration", sh]).output().unwrap();
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("133;A") && text.contains("]7;file://"),
            "{sh}"
        );
    }
    let bad = s.cmd(&["shell-integration", "tcsh"]).output().unwrap();
    assert_eq!(bad.status.code(), Some(2));
    // The bash snippet loads cleanly in an interactive shell.
    let script = s
        .cmd(&["shell-integration", "bash"])
        .output()
        .unwrap()
        .stdout;
    let f = s.dir.path().join("si.bash");
    std::fs::write(&f, script).unwrap();
    let st = std::process::Command::new("bash")
        .args(["--norc", "-n", f.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(st.success(), "bash -n");
}

#[test]
fn audit_rotation_keeps_the_chain_and_offline_records_join_it() {
    let s = Session::new();
    std::fs::write(
        s.dir.path().join("config.toml"),
        "[security.audit]\nmax_bytes = 1500\nmax_segments = 50\n",
    )
    .unwrap();
    let mut r = rpc(&s);
    for i in 0..15 {
        r.call(
            "policy.add",
            json!({"tool": "Bash", "command_regex": format!("^echo {i}$"), "effect": "deny", "note": "x".repeat(80)}),
        )
        .unwrap();
    }
    // Offline record (no server involvement): a remote machine add.
    let add = s
        .cmd(&["machine", "add", "devbox", "me@devbox.invalid"])
        .output()
        .unwrap();
    assert!(
        add.status.success(),
        "{}",
        String::from_utf8_lossy(&add.stderr)
    );
    r.call("policy.add", json!({"tool": "Read", "effect": "ask"}))
        .unwrap();
    let v = r.call("audit.verify", json!({})).unwrap();
    assert_eq!(v["ok"], true, "{v}");
    assert!(
        !v["segments"].as_array().unwrap().is_empty(),
        "rotated: {v}"
    );
    let tail = r.call("audit.tail", json!({"limit": 500})).unwrap();
    let types: Vec<&str> = tail["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["type"].as_str())
        .collect();
    assert!(types.contains(&"audit.rotated"), "{types:?}");
    assert!(types.contains(&"remote.machine_added"), "{types:?}");
    assert_eq!(
        types.iter().filter(|t| **t == "policy.rule_added").count(),
        16,
        "{types:?}"
    );
    let doc = s.cmd(&["doctor", "--audit"]).output().unwrap();
    assert!(
        doc.status.success(),
        "{}",
        String::from_utf8_lossy(&doc.stdout)
    );
}

#[test]
fn tab_renumber_closes_gaps_in_tab_order() {
    let s = Session::new();
    let root = s.workspace("sleep 1000");
    let ws = s.pane(&root)["workspace"].as_str().unwrap().to_string();
    let mut r = rpc(&s);
    let mut tabs = vec![];
    for _ in 0..2 {
        let t = r
            .call(
                "tab.create",
                json!({"workspace": ws, "command": ["sleep", "1000"]}),
            )
            .unwrap();
        tabs.push(t["tab"]["id"].as_str().unwrap().to_string());
    }
    r.call("tab.close", json!({"tab": tabs[0]})).unwrap();
    // The tab goes once its pane's process has exited, which a slow runner reports later.
    let t0 = std::time::Instant::now();
    loop {
        let left = r.call("tab.list", json!({"workspace": ws})).unwrap();
        if left["tabs"].as_array().unwrap().len() == 2 {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "tab never closed: {left}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = s.json(&["tab", "renumber", &ws]);
    assert_eq!(out["changed"], 1, "{out}");
    let numbers: Vec<u64> = out["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["number"].as_u64().unwrap())
        .collect();
    assert_eq!(numbers, vec![1, 2]);
    assert!(out["tabs"][1]["handle"].as_str().unwrap().ends_with(":t2"));
    // The next tab continues after the renumbered ones.
    let t = r
        .call(
            "tab.create",
            json!({"workspace": ws, "command": ["sleep", "1000"]}),
        )
        .unwrap();
    assert_eq!(t["tab"]["number"], 3);
    assert_eq!(events_of(&mut r, "tab.renumbered").len(), 1);
}
