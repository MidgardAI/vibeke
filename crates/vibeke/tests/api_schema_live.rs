//! The schema registry against a real server: results of live calls and the payloads of the
//! events they produce must validate against `vk_server::api_schema`, and the params the test
//! sends must validate too. Keeps `api_schema.rs` honest about what the handlers return.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::Duration;
use vk_server::api_schema::{validate_event, validate_params, validate_result};

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new() -> Self {
        Session {
            dir: tempfile::Builder::new()
                .prefix("vkschema")
                .tempdir_in("/tmp")
                .unwrap(),
        }
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"));
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
        ] {
            c.env_remove(k);
        }
        c.arg("--json").args(args);
        c
    }
    fn socket(&self) -> std::path::PathBuf {
        self.dir.path().join("run/default/vibeke.sock")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

struct Rpc {
    rd: BufReader<UnixStream>,
    wr: UnixStream,
    id: u64,
}

impl Rpc {
    fn connect(path: &std::path::Path) -> Rpc {
        let s = UnixStream::connect(path).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        Rpc {
            rd: BufReader::new(s.try_clone().unwrap()),
            wr: s,
            id: 0,
        }
    }
    fn call(&mut self, method: &str, params: &Value) -> Result<Value, Value> {
        self.id += 1;
        let req = json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params});
        writeln!(self.wr, "{req}").unwrap();
        loop {
            let mut line = String::new();
            assert!(
                self.rd.read_line(&mut line).unwrap() > 0,
                "closed in {method}"
            );
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == json!(self.id) {
                return match v.get("error") {
                    Some(e) if !e.is_null() => Err(e.clone()),
                    _ => Ok(v["result"].clone()),
                };
            }
        }
    }
}

#[test]
fn live_results_and_events_match_the_registry() {
    let s = Session::new();
    // Start the server (the CLI spawns it on demand).
    let out = s.cmd(&["server", "status"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut rpc = Rpc::connect(&s.socket());
    let problems = std::cell::RefCell::new(Vec::<String>::new());
    let check = |rpc: &mut Rpc, method: &str, params: Value| -> Option<Value> {
        for p in validate_params(method, &params) {
            problems
                .borrow_mut()
                .push(format!("{method} params {params}: {p}"));
        }
        match rpc.call(method, &params) {
            Ok(r) => {
                for p in validate_result(method, &r) {
                    problems.borrow_mut().push(format!("{method} result: {p}"));
                }
                Some(r)
            }
            Err(e) => {
                problems.borrow_mut().push(format!("{method} failed: {e}"));
                None
            }
        }
    };
    check(
        &mut rpc,
        "client.hello",
        json!({"client": "schema-test", "version": "0", "api": "vibeke/1", "kind": "cli"}),
    );
    check(&mut rpc, "server.status", json!({}));
    check(&mut rpc, "api.methods", json!({}));
    check(&mut rpc, "api.schema", json!({"method": "pane.split"}));
    let ws = check(
        &mut rpc,
        "workspace.create",
        json!({"cwd": "/tmp", "name": "schema"}),
    );
    let pane = ws
        .as_ref()
        .map(|w| w["root_pane"]["id"].as_str().unwrap().to_string())
        .unwrap_or_default();
    let wsid = ws
        .as_ref()
        .map(|w| w["workspace"]["id"].as_str().unwrap().to_string())
        .unwrap_or_default();
    let tab = ws
        .as_ref()
        .map(|w| w["tab"]["id"].as_str().unwrap().to_string())
        .unwrap_or_default();
    check(&mut rpc, "workspace.list", json!({}));
    check(&mut rpc, "workspace.get", json!({"workspace": wsid}));
    check(
        &mut rpc,
        "workspace.rename",
        json!({"workspace": wsid, "name": "schema2"}),
    );
    check(&mut rpc, "tab.list", json!({"workspace": wsid}));
    check(&mut rpc, "tab.rename", json!({"tab": tab, "title": "t"}));
    check(
        &mut rpc,
        "tab.create",
        json!({"workspace": wsid, "cwd": "/tmp"}),
    );
    check(&mut rpc, "pane.list", json!({}));
    check(&mut rpc, "pane.get", json!({"pane": pane}));
    check(&mut rpc, "pane.rename", json!({"pane": pane, "title": "p"}));
    check(&mut rpc, "pane.pin", json!({"pane": pane, "pinned": true}));
    check(&mut rpc, "pane.mark_unread", json!({"pane": pane}));
    check(&mut rpc, "pane.mark_seen", json!({"pane": pane}));
    check(
        &mut rpc,
        "pane.send_text",
        json!({"pane": pane, "text": "echo hi\n"}),
    );
    check(
        &mut rpc,
        "pane.read",
        json!({"pane": pane, "source": "visible"}),
    );
    check(
        &mut rpc,
        "pane.split",
        json!({"pane": pane, "direction": "right"}),
    );
    check(&mut rpc, "pane.zoom", json!({"pane": pane}));
    check(&mut rpc, "session.snapshot", json!({}));
    check(&mut rpc, "group.create", json!({"name": "g"}));
    check(
        &mut rpc,
        "group.add",
        json!({"group": "g", "workspace": wsid}),
    );
    check(&mut rpc, "group.list", json!({}));
    check(&mut rpc, "layout.list", json!({}));
    check(&mut rpc, "layout.export", json!({"workspace": wsid}));
    check(
        &mut rpc,
        "notes.set",
        json!({"workspace": wsid, "text": "x"}),
    );
    check(&mut rpc, "notes.get", json!({"workspace": wsid}));
    check(&mut rpc, "notification.send", json!({"title": "t"}));
    check(&mut rpc, "notification.list", json!({}));
    check(&mut rpc, "notification.config", json!({}));
    check(&mut rpc, "theme.get", json!({}));
    check(&mut rpc, "client.list", json!({}));
    check(&mut rpc, "agent.list", json!({}));
    check(&mut rpc, "agent.harnesses", json!({}));
    check(&mut rpc, "agent.manifests", json!({}));
    check(&mut rpc, "agent.resumable", json!({}));
    check(&mut rpc, "interaction.list", json!({}));
    check(&mut rpc, "task.list", json!({}));
    check(&mut rpc, "preview.list", json!({}));
    check(&mut rpc, "preview.status", json!({}));
    check(&mut rpc, "screenshot.list", json!({}));
    check(&mut rpc, "plugin.list", json!({}));
    check(&mut rpc, "desk.status", json!({}));
    check(&mut rpc, "assistant.status", json!({}));
    check(&mut rpc, "attention.list", json!({}));
    check(&mut rpc, "sandbox.status", json!({}));
    check(&mut rpc, "compat.status", json!({}));
    check(&mut rpc, "browser.list", json!({}));
    check(&mut rpc, "browser.pane.list", json!({}));
    // A repository for the fs/git/worktree methods.
    let repo = s.dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    let run_git = |args: &[&str]| {
        let st = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(st.status.success(), "git {args:?}");
    };
    run_git(&["init", "-q"]);
    run_git(&["add", "."]);
    run_git(&["commit", "-q", "-m", "init"]);
    std::fs::write(repo.join("a.txt"), "two\n").unwrap();
    let repo_s = repo.to_string_lossy().to_string();
    let rws = check(&mut rpc, "workspace.create", json!({"cwd": repo_s}));
    let rpane = rws
        .as_ref()
        .map(|w| w["root_pane"]["id"].as_str().unwrap().to_string())
        .unwrap_or_default();
    check(&mut rpc, "worktree.list", json!({"cwd": repo_s}));
    check(&mut rpc, "worktree.repo_root", json!({"cwd": repo_s}));
    check(&mut rpc, "git.status", json!({"pane": rpane}));
    check(
        &mut rpc,
        "git.diff",
        json!({"pane": rpane, "file": "a.txt"}),
    );
    check(&mut rpc, "git.log", json!({"pane": rpane}));
    check(&mut rpc, "fs.list", json!({"pane": rpane}));
    check(&mut rpc, "fs.read", json!({"pane": rpane, "path": "a.txt"}));
    check(&mut rpc, "draft.list", json!({"all": true}));
    check(&mut rpc, "search.query", json!({"q": "hi"}));
    check(&mut rpc, "status.segments", json!({}));
    check(&mut rpc, "server.reload_config", json!({}));
    let ev = check(&mut rpc, "events.read", json!({"limit": 500}));
    if let Some(ev) = ev {
        let events = ev["events"].as_array().unwrap();
        assert!(!events.is_empty());
        for e in events {
            for p in validate_event(e) {
                problems
                    .borrow_mut()
                    .push(format!("event {} ({}): {p}", e["type"], e["seq"]));
            }
        }
    }
    check(
        &mut rpc,
        "workspace.close",
        json!({"workspace": wsid, "force": true}),
    );
    let problems = problems.into_inner();
    assert!(
        problems.is_empty(),
        "registry disagrees with the server:\n{}",
        problems.join("\n")
    );
}
