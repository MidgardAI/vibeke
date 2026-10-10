//! The schema registry against a real server: results of live calls and the payloads of the
//! events they produce must validate against `vk_server::api_schema`, and the params the test
//! sends must validate too. Keeps `api_schema.rs` honest about what the handlers return.
//!
//! Every value is checked twice: with the registry's own validator and with a real JSON Schema
//! 2020-12 validator (`jsonschema`) against the *emitted* bundle (`api_schema::bundle()`, the
//! same document as `docs/api/vibeke-1.schema.json`), so the two can never disagree silently
//! (e.g. about `null` in optional fields).

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use vk_server::api_schema::{bundle, validate_event, validate_params, validate_result};

/// The emitted schema `s` with the bundle's `$defs`, compiled as JSON Schema 2020-12.
fn compile(s: &Value, defs: &Value) -> jsonschema::Validator {
    let mut root = serde_json::Map::new();
    root.insert(
        "$schema".into(),
        json!("https://json-schema.org/draft/2020-12/schema"),
    );
    root.insert("$defs".into(), defs.clone());
    root.insert("allOf".into(), json!([s]));
    jsonschema::draft202012::new(&Value::Object(root))
        .unwrap_or_else(|e| panic!("schema does not compile: {e}\n{s}"))
}

/// JSON Schema 2020-12 problems of `v` against `schema` (with the bundle's `$defs`).
fn json_schema_problems(schema: &Value, v: &Value) -> Vec<String> {
    let b = bundle();
    compile(schema, &b["$defs"])
        .iter_errors(v)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect()
}

fn method_schema(method: &str, part: &str) -> Value {
    bundle()["x-methods"][method][part].clone()
}

fn event_problems_2020(e: &Value) -> Vec<String> {
    let b = bundle();
    let mut out = json_schema_problems(&json!({"$ref": "#/$defs/Event"}), e);
    let t = e["type"].as_str().unwrap_or_default();
    let ev = &b["x-events"][t];
    if ev.is_null() {
        out.push(format!("{t}: not in the bundle"));
        return out;
    }
    out.extend(json_schema_problems(&ev["subject"], &e["subject"]));
    out.extend(json_schema_problems(&ev["data"], &e["data"]));
    out
}

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
        // An ephemeral proxy port (never the machine-wide default) and no real browser.
        // Task worktrees inside the session dir, never under the real home.
        let _ = std::fs::write(
            d.join("config.toml"),
            format!(
                "[preview]\nproxy_port = 0\n\n[tasks]\nroot = \"{}\"\nfetch_before_create = false\n",
                d.join("worktrees").display()
            ),
        );
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env("VIBEKE_NO_OPEN", "1");
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
        for p in json_schema_problems(&method_schema(method, "params"), &params) {
            problems
                .borrow_mut()
                .push(format!("{method} params {params} (JSON Schema): {p}"));
        }
        match rpc.call(method, &params) {
            Ok(r) => {
                for p in validate_result(method, &r) {
                    problems.borrow_mut().push(format!("{method} result: {p}"));
                }
                for p in json_schema_problems(&method_schema(method, "result"), &r) {
                    problems
                        .borrow_mut()
                        .push(format!("{method} result (JSON Schema): {p}\n  {r}"));
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
    // A task with a worktree, finished with a JSON boolean (the typed form of `remove_worktree`).
    let t = check(
        &mut rpc,
        "task.create",
        json!({"title": "schema task", "repo": repo_s, "setup": false, "root": s.dir.path().join("worktrees").to_string_lossy()}),
    );
    if let Some(t) = t {
        let tid = t["task"]["id"].as_str().unwrap().to_string();
        let wt = t["task"]["worktree_path"].as_str().map(str::to_string);
        assert!(
            wt.as_deref().is_none_or(|w| w.contains("vkschema")),
            "the task worktree must live in the test's session dir: {wt:?}"
        );
        check(&mut rpc, "task.get", json!({"task": tid}));
        let f = check(
            &mut rpc,
            "task.finish",
            json!({"task": tid, "remove_worktree": true}),
        );
        if let (Some(_), Some(wt)) = (f, wt) {
            let t0 = std::time::Instant::now();
            while Path::new(&wt).exists() && t0.elapsed() < Duration::from_secs(20) {
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(
                !Path::new(&wt).exists(),
                "remove_worktree: true (a JSON boolean) removes the worktree"
            );
        }
    }
    // Previews: preview.url before any proxy origin exists (proxy_url: null), a tls_origin
    // declaration, a TLS proxy open, and the statuses around it.
    let app = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let app_port = app.local_addr().unwrap().port();
    let d = check(
        &mut rpc,
        "preview.declare",
        json!({"port": app_port, "path": "/x", "label": "schema", "tls_origin": true}),
    );
    let ph = d
        .as_ref()
        .map(|d| d["preview"]["handle"].as_str().unwrap().to_string())
        .unwrap_or_default();
    let u = check(&mut rpc, "preview.url", json!({"preview": ph}));
    assert_eq!(
        u.as_ref().map(|u| u["proxy_url"].clone()),
        Some(Value::Null)
    );
    check(&mut rpc, "preview.get", json!({"preview": ph}));
    check(&mut rpc, "preview.status", json!({}));
    let o = check(
        &mut rpc,
        "preview.open",
        json!({"preview": ph, "mode": "proxy", "no_open": true, "tls_origin": true}),
    );
    if let Some(o) = &o {
        assert_eq!(o["opened_in"], "proxy", "{o}");
        assert_eq!(o["tls_origin"], true, "{o}");
        assert!(o["ca"]["sha256"].is_string(), "{o}");
    }
    check(&mut rpc, "preview.url", json!({"preview": ph}));
    check(&mut rpc, "preview.status", json!({}));
    check(&mut rpc, "preview.list", json!({"status": "all"}));
    check(&mut rpc, "preview.forget", json!({"preview": ph}));
    drop(app);
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
            for p in event_problems_2020(e) {
                problems.borrow_mut().push(format!(
                    "event {} ({}) (JSON Schema): {p}",
                    e["type"], e["seq"]
                ));
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

/// The registry's validator agrees with JSON Schema 2020-12 on the emitted schema, on values
/// that probe the differences that matter: `null` in optional and nullable fields, boolean
/// literals, enums, unions and missing required fields.
#[test]
fn the_registry_validator_matches_the_emitted_schema() {
    let cases: &[(&str, &str, Value)] = &[
        (
            "preview.url",
            "result",
            json!({"remote_url": "u", "profile_url": "p", "proxy_url": null}),
        ),
        (
            "preview.url",
            "result",
            json!({"remote_url": "u", "profile_url": "p", "proxy_url": "x"}),
        ),
        (
            "preview.url",
            "result",
            json!({"remote_url": "u", "profile_url": "p"}),
        ),
        (
            "preview.url",
            "result",
            json!({"remote_url": "u", "profile_url": "p", "proxy_url": 1}),
        ),
        (
            "task.finish",
            "params",
            json!({"task": "t1", "remove_worktree": true}),
        ),
        (
            "task.finish",
            "params",
            json!({"task": "t1", "remove_worktree": "ask"}),
        ),
        (
            "task.finish",
            "params",
            json!({"task": "t1", "remove_worktree": "true"}),
        ),
        (
            "task.finish",
            "params",
            json!({"task": "t1", "remove_worktree": null}),
        ),
        ("events.subscribe", "params", json!({"types": null})),
        ("events.subscribe", "params", json!({"types": ["a.*"]})),
        ("pane.get", "result", json!({"pane": {}, "run": null})),
        (
            "task.review.request_reviewer",
            "result",
            json!({"request": {}, "prompt": "", "prompt_digest": "", "harness": "claude", "subject": "s", "requires_confirmation": true, "label": "", "uses_provider": "", "confirm_with": {"method": "m", "params": {"request": "r", "prompt_digest": "d"}}}),
        ),
        (
            "task.review.request_reviewer",
            "result",
            json!({"request": {}, "prompt": "", "prompt_digest": "", "harness": "claude", "subject": "s", "requires_confirmation": "true", "label": "", "uses_provider": "", "confirm_with": {"method": "m", "params": {"request": "r", "prompt_digest": "d"}}}),
        ),
        (
            "preview.open",
            "params",
            json!({"preview": "v1", "mode": "window"}),
        ),
        (
            "preview.open",
            "params",
            json!({"preview": "v1", "mode": "profile"}),
        ),
        (
            "preview.open",
            "params",
            json!({"preview": "v1", "mode": "proxy", "tls_origin": true, "no_open": true}),
        ),
        (
            "preview.declare",
            "params",
            json!({"port": 5173, "tls_origin": true}),
        ),
        (
            "preview.declare",
            "params",
            json!({"port": 5173, "tls_origin": "yes"}),
        ),
    ];
    for (m, part, v) in cases {
        let ours = if *part == "result" {
            validate_result(m, v)
        } else {
            validate_params(m, v)
        };
        let theirs = json_schema_problems(&method_schema(m, part), v);
        assert_eq!(
            ours.is_empty(),
            theirs.is_empty(),
            "{m} {part} {v}: registry {ours:?} vs JSON Schema {theirs:?}"
        );
    }
    // And the expected verdicts.
    let ok = |m: &str, part: &str, v: Value| {
        json_schema_problems(&method_schema(m, part), &v).is_empty()
    };
    assert!(ok(
        "preview.url",
        "result",
        json!({"remote_url": "u", "profile_url": "p", "proxy_url": null})
    ));
    assert!(!ok(
        "task.finish",
        "params",
        json!({"task": "t", "remove_worktree": "true"})
    ));
    assert!(ok(
        "task.finish",
        "params",
        json!({"task": "t", "remove_worktree": false})
    ));
    assert!(!ok("events.subscribe", "params", json!({"types": null})));
    assert!(!ok(
        "preview.open",
        "params",
        json!({"preview": "v1", "mode": "profile"})
    ));
    assert!(ok(
        "preview.open",
        "params",
        json!({"preview": "v1", "mode": "window"})
    ));
}
