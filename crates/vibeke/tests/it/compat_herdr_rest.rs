//! Herdr 0.9.3 surfaces closed after the plugin harness runs, end to end against isolated
//! sessions: directional pane navigation (`pane.neighbor`, `pane.edges`,
//! `pane.focus_direction`), `tab.get`, `session.snapshot` and `ping` shapes, Herdr's
//! `events.wait {match_event}`, the small pane and server methods, `workspace.move_block`,
//! `worktree.remove`, and the shim's local `plugin` commands and `--version`.
//!
//! Temp VIBEKE_* dirs only; nothing touches a real Herdr, its config or sockets.

use crate::support::Session;
use crate::support::plugins::{git, session, write};
use serde_json::{Value, json};
use std::path::Path;

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

/// One Herdr socket method through the native `compat.herdr.call` (`{result}` or `{error}`).
fn call(s_: &Session, method: &str, params: Value) -> Value {
    s_.api(
        "compat.herdr.call",
        json!({"method": method, "params": params}),
    )
}

fn ok(s_: &Session, method: &str, params: Value) -> Value {
    let r = call(s_, method, params.clone());
    assert!(r.get("error").is_none(), "{method} {params}: {r}");
    r["result"].clone()
}

fn err_code(s_: &Session, method: &str, params: Value) -> String {
    let r = call(s_, method, params.clone());
    assert!(
        r.get("result").is_none(),
        "{method} {params} should fail: {r}"
    );
    s(&r["error"]["code"])
}

/// `[p1 | (p2 / p3)]` in a new workspace: `(workspace, tab, p1, p2, p3)`.
fn three_panes(s_: &Session) -> (String, String, String, String, String) {
    let created = s_.herdr(&["workspace", "create", "--cwd", "/tmp"]);
    let ws = s(&created["workspace"]["workspace_id"]);
    let tab = s(&created["tab"]["tab_id"]);
    let p1 = s(&created["root_pane"]["pane_id"]);
    let p2 = s(&s_.herdr(&["pane", "split", &p1, "--direction", "right"])["pane"]["pane_id"]);
    let p3 = s(&s_.herdr(&["pane", "split", &p2, "--direction", "down"])["pane"]["pane_id"]);
    assert!(!p2.is_empty() && !p3.is_empty(), "{p2} {p3}");
    (ws, tab, p1, p2, p3)
}

#[test]
fn herdr_rest_navigation_snapshot_and_small_methods() {
    let s_ = session("");
    let (ws, tab, p1, p2, p3) = three_panes(&s_);

    // `pane.neighbor`: Herdr's nearest pane on that side, by edge distance, overlap, centre
    // and layout order; no `neighbor_pane_id` at an outer edge.
    let n = ok(
        &s_,
        "pane.neighbor",
        json!({"pane_id": p1, "direction": "right"}),
    );
    assert_eq!(n["type"], "pane_neighbor", "{n}");
    assert_eq!(n["neighbor"]["pane_id"], p1.as_str(), "{n}");
    assert_eq!(n["neighbor"]["direction"], "right", "{n}");
    assert_eq!(n["neighbor"]["neighbor_pane_id"], p2.as_str(), "{n}");
    assert_eq!(n["neighbor"]["layout"]["tab_id"], tab.as_str(), "{n}");
    let n = ok(
        &s_,
        "pane.neighbor",
        json!({"pane_id": p3, "direction": "up"}),
    );
    assert_eq!(n["neighbor"]["neighbor_pane_id"], p2.as_str(), "{n}");
    let n = ok(
        &s_,
        "pane.neighbor",
        json!({"pane_id": p3, "direction": "left"}),
    );
    assert_eq!(n["neighbor"]["neighbor_pane_id"], p1.as_str(), "{n}");
    let n = ok(
        &s_,
        "pane.neighbor",
        json!({"pane_id": p1, "direction": "left"}),
    );
    assert!(n["neighbor"].get("neighbor_pane_id").is_none(), "{n}");
    assert_eq!(
        err_code(
            &s_,
            "pane.neighbor",
            json!({"pane_id": p1, "direction": "sideways"})
        ),
        "invalid_params"
    );
    assert_eq!(
        err_code(
            &s_,
            "pane.neighbor",
            json!({"pane_id": "w99:p99", "direction": "up"})
        ),
        "pane_not_found"
    );
    // The CLI sends the same request (`cli:pane:neighbor`).
    let n = s_.herdr(&["pane", "neighbor", "--pane", &p2, "--direction", "down"]);
    assert_eq!(n["type"], "pane_neighbor", "{n}");
    assert_eq!(n["neighbor"]["neighbor_pane_id"], p3.as_str(), "{n}");

    // `pane.edges`: which outer edges of the tab area a pane touches.
    let e = ok(&s_, "pane.edges", json!({"pane_id": p1}));
    assert_eq!(e["type"], "pane_edges", "{e}");
    let edges = |v: &Value| {
        let x = &v["edges"];
        [&x["left"], &x["right"], &x["up"], &x["down"]].map(|b| b.as_bool().unwrap())
    };
    assert_eq!(edges(&e), [true, false, true, true], "{e}");
    let e = ok(&s_, "pane.edges", json!({"pane_id": p3}));
    assert_eq!(edges(&e), [false, true, false, true], "{e}");
    assert!(e["edges"]["layout"]["panes"].is_array(), "{e}");

    // `pane.focus_direction`: moves the focus, or reports `no_neighbor`.
    let f = ok(
        &s_,
        "pane.focus_direction",
        json!({"pane_id": p1, "direction": "right"}),
    );
    assert_eq!(f["type"], "pane_focus_direction", "{f}");
    assert_eq!(f["focus"]["changed"], true, "{f}");
    assert_eq!(f["focus"]["source_pane_id"], p1.as_str(), "{f}");
    assert_eq!(f["focus"]["focused_pane_id"], p2.as_str(), "{f}");
    assert!(f["focus"].get("reason").is_none(), "{f}");
    let f = ok(
        &s_,
        "pane.focus_direction",
        json!({"pane_id": p2, "direction": "right"}),
    );
    assert_eq!(f["focus"]["changed"], false, "{f}");
    assert_eq!(f["focus"]["reason"], "no_neighbor", "{f}");

    // `tab.get`.
    let t = s_.herdr(&["tab", "get", &tab]);
    assert_eq!(t["type"], "tab_info", "{t}");
    assert_eq!(t["tab"]["tab_id"], tab.as_str(), "{t}");
    assert_eq!(t["tab"]["workspace_id"], ws.as_str(), "{t}");
    assert_eq!(
        err_code(&s_, "tab.get", json!({"tab_id": "w99:t9"})),
        "tab_not_found"
    );

    // `session.snapshot` is wrapped in `snapshot`, with the integer protocol; `ping` too.
    let snap = s_.herdr(&["api", "snapshot"]);
    assert_eq!(snap["type"], "session_snapshot", "{snap}");
    let sn = &snap["snapshot"];
    assert_eq!(sn["version"], "0.9.3", "{snap}");
    assert_eq!(sn["protocol"], 22, "{snap}");
    assert_eq!(
        sn["layouts"].as_array().unwrap().len(),
        sn["tabs"].as_array().unwrap().len(),
        "one layout per tab: {snap}"
    );
    assert!(
        sn["panes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["pane_id"] == p3.as_str()),
        "{snap}"
    );
    let pong = ok(&s_, "ping", json!({}));
    assert_eq!(pong["type"], "pong", "{pong}");
    assert_eq!(pong["protocol"], 22, "{pong}");
    assert_eq!(pong["version"], "0.9.3", "{pong}");

    // Herdr's `events.wait`: a status the pane already has matches at once.
    let w = ok(
        &s_,
        "events.wait",
        json!({"match_event": {"event": "pane_agent_status_changed", "pane_id": p1, "agent_status": "unknown"}, "timeout_ms": 5000}),
    );
    assert_eq!(w["type"], "wait_matched", "{w}");
    assert_eq!(w["event"]["event"], "pane_agent_status_changed", "{w}");
    assert_eq!(
        w["event"]["data"]["type"], "pane_agent_status_changed",
        "{w}"
    );
    assert_eq!(w["event"]["data"]["pane_id"], p1.as_str(), "{w}");
    assert_eq!(w["event"]["data"]["workspace_id"], ws.as_str(), "{w}");
    assert_eq!(w["event"]["data"]["agent_status"], "unknown", "{w}");
    assert_eq!(
        err_code(
            &s_,
            "events.wait",
            json!({"match_event": {"event": "pane_agent_status_changed", "pane_id": p1, "agent_status": "working"}, "timeout_ms": 300}),
        ),
        "timeout"
    );
    assert_eq!(
        err_code(
            &s_,
            "events.wait",
            json!({"match_event": {"event": "tab_created"}}),
        ),
        "unsupported_event_wait_match"
    );

    // Small pane methods.
    let r = ok(
        &s_,
        "pane.input.set",
        json!({"pane_id": p1, "right_click": "pane"}),
    );
    assert_eq!(r["type"], "ok", "{r}");
    assert_eq!(
        err_code(
            &s_,
            "pane.input.set",
            json!({"pane_id": p1, "right_click": "menu"})
        ),
        "invalid_params"
    );
    assert_eq!(
        err_code(
            &s_,
            "pane.release_agent",
            json!({"pane_id": p1, "source": "hook", "agent": "  "})
        ),
        "invalid_agent"
    );
    let r = ok(
        &s_,
        "pane.release_agent",
        json!({"pane_id": p1, "source": "hook", "agent": "custom"}),
    );
    assert_eq!(r["type"], "ok", "{r}");
    let r = ok(&s_, "pane.clear_agent_authority", json!({"pane_id": p1}));
    assert_eq!(r["type"], "ok", "{r}");
    let r = ok(
        &s_,
        "pane.scroll",
        json!({"pane_id": p1, "offset_from_bottom": 0}),
    );
    assert_eq!(r["type"], "pane_info", "{r}");
    assert_eq!(r["pane"]["pane_id"], p1.as_str(), "{r}");

    // Herdr's answers where Vibeke has none of the state, and explicit `unsupported`.
    assert_eq!(
        err_code(&s_, "release_notes.dismiss", json!({"version": "0.9.3"})),
        "stale_release_notes"
    );
    assert_eq!(
        err_code(
            &s_,
            "product_announcement.dismiss",
            json!({"version": "0.9.3", "id": "x"})
        ),
        "stale_announcement"
    );
    assert_eq!(
        err_code(&s_, "server.ssh_agent.register", json!({})),
        "connection_local_only"
    );
    let r = call(&s_, "pane.clear", json!({"pane_id": p1}));
    assert_eq!(r["error"]["code"], "unsupported", "{r}");
    assert!(
        s(&r["error"]["message"]).contains("does not serve it"),
        "{r}"
    );

    // `workspace.move_block`: the block lands before the named workspace.
    let w2 = s(&s_.herdr(&["workspace", "create", "--cwd", "/tmp"])["workspace"]["workspace_id"]);
    let l = ok(
        &s_,
        "workspace.move_block",
        json!({"workspace_ids": [w2], "before_workspace_id": ws}),
    );
    assert_eq!(l["type"], "workspace_list", "{l}");
    let order: Vec<String> = l["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| s(&w["workspace_id"]))
        .collect();
    let (a, b) = (
        order.iter().position(|w| w == &w2).unwrap(),
        order.iter().position(|w| w == &ws).unwrap(),
    );
    assert_eq!(b, a + 1, "{order:?}");
    assert_eq!(
        err_code(&s_, "workspace.move_block", json!({"workspace_ids": []})),
        "workspace_move_block_failed"
    );
    assert_eq!(
        err_code(
            &s_,
            "workspace.move_block",
            json!({"workspace_ids": [w2], "before_workspace_id": w2})
        ),
        "workspace_move_block_failed"
    );
}

#[test]
fn herdr_rest_agent_methods_need_an_agent() {
    let s_ = session("");
    let (_, _, p1, _, _) = three_panes(&s_);
    assert_eq!(
        err_code(&s_, "agent.focus", json!({"target": p1})),
        "agent_not_found"
    );
    assert_eq!(
        err_code(
            &s_,
            "agent.send_keys",
            json!({"target": "nobody", "keys": ["Enter"]})
        ),
        "agent_not_found"
    );
    assert_eq!(
        err_code(&s_, "agent.send_keys", json!({"target": "nobody"})),
        "agent_not_found",
        "the target is resolved first"
    );
}

fn repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    write(&dir.join("README.md"), "hello\n");
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

#[test]
fn herdr_rest_worktree_remove() {
    let s_ = session("");
    let main = s_.path("proj");
    repo(&main);
    let checkout = s_.path("proj-feat");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            checkout.to_str().unwrap(),
        ],
    );
    let dirty = s_.path("proj-dirty");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "dirty",
            dirty.to_str().unwrap(),
        ],
    );
    write(&dirty.join("README.md"), "changed\n");

    // The main checkout is not a linked worktree.
    let mw = ok(
        &s_,
        "workspace.create",
        json!({"cwd": main.to_str().unwrap()}),
    );
    let main_ws = s(&mw["workspace"]["workspace_id"]);
    assert_eq!(
        err_code(&s_, "worktree.remove", json!({"workspace_id": main_ws})),
        "not_linked_worktree"
    );
    assert_eq!(
        err_code(&s_, "worktree.remove", json!({"workspace_id": "w99"})),
        "workspace_not_found"
    );

    // A clean linked checkout: removed, its workspace closed.
    let o = ok(
        &s_,
        "worktree.open",
        json!({"path": checkout.to_str().unwrap()}),
    );
    let ws = s(&o["workspace"]["workspace_id"]);
    let r = ok(&s_, "worktree.remove", json!({"workspace_id": ws}));
    assert_eq!(r["type"], "worktree_removed", "{r}");
    assert_eq!(r["workspace_id"], ws.as_str(), "{r}");
    assert_eq!(r["path"], checkout.to_str().unwrap(), "{r}");
    assert_eq!(r["forced"], false, "{r}");
    assert!(!checkout.exists(), "checkout removed");
    let wl = ok(&s_, "workspace.list", json!({}));
    assert!(
        !wl["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["workspace_id"] == ws.as_str()),
        "{wl}"
    );
    let evs = s_.events("worktree.removed");
    assert!(
        evs.iter().any(
            |e| e["data"]["forced"] == false && e["data"]["path"] == checkout.to_str().unwrap()
        ),
        "{evs:?}"
    );

    // A dirty checkout needs `force`.
    let o = ok(
        &s_,
        "worktree.open",
        json!({"path": dirty.to_str().unwrap()}),
    );
    let ws = s(&o["workspace"]["workspace_id"]);
    assert_eq!(
        err_code(&s_, "worktree.remove", json!({"workspace_id": ws})),
        "dirty_worktree_requires_force"
    );
    assert!(dirty.exists());
    let r = ok(
        &s_,
        "worktree.remove",
        json!({"workspace_id": ws, "force": true}),
    );
    assert_eq!(r["forced"], true, "{r}");
    assert!(!dirty.exists());
}

fn write_plugin(dir: &Path) {
    write(
        &dir.join("herdr-plugin.toml"),
        r#"id = "acme.rest"
name = "Rest"
version = "0.2.0"
platforms = ["linux", "macos"]

[[actions]]
id = "zeta"
title = "Zeta"
command = ["sh", "-c", "true"]

[[actions]]
id = "alpha"
title = "Alpha"
description = "First"
contexts = ["pane"]
command = ["sh", "-c", "echo alpha"]
"#,
    );
}

/// stdout, stderr and exit code of `vibeke compat herdr <args>` (no `--json` parsing).
fn shim(s_: &Session, args: &[&str]) -> (String, String, i32) {
    let mut a = vec!["compat", "herdr"];
    a.extend(args);
    let out = s_.cmd(&a).output().unwrap();
    (
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn herdr_rest_plugin_registry_cli_prints_like_herdr() {
    let s_ = session("");
    // `--version` is Herdr's exact line.
    let (out, _, code) = shim(&s_, &["--version"]);
    assert_eq!((out.as_str(), code), ("herdr 0.9.3\n", 0));

    let (out, _, code) = shim(&s_, &["plugin", "list"]);
    assert_eq!((out.as_str(), code), ("No plugins installed.\n", 0));

    let src = s_.path("src/rest");
    write_plugin(&src);
    let (out, err, code) = shim(&s_, &["plugin", "link", src.to_str().unwrap()]);
    assert_eq!(code, 0, "{err}");
    let r: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(r["id"], "cli:plugin", "{r}");
    let p = &r["result"]["plugin"];
    assert_eq!(r["result"]["type"], "plugin_linked", "{r}");
    assert_eq!(p["plugin_id"], "acme.rest", "{r}");
    assert_eq!(p["name"], "Rest", "{r}");
    assert_eq!(p["version"], "0.2.0", "{r}");
    assert_eq!(p["enabled"], true, "{r}");
    assert_eq!(p["plugin_root"], src.to_str().unwrap(), "{r}");
    assert!(s(&p["manifest_path"]).ends_with("herdr-plugin.toml"), "{r}");
    assert_eq!(p["source"], json!({"kind": "local"}), "{r}");
    // Herdr's manifest lists: actions by id, contexts only when declared.
    let ids: Vec<String> = p["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| s(&a["id"]))
        .collect();
    assert_eq!(ids, vec!["alpha", "zeta"], "{r}");
    assert_eq!(p["actions"][0]["contexts"], json!(["pane"]), "{r}");
    assert!(p["actions"][1].get("contexts").is_none(), "{r}");

    // Human list and `--json`.
    let (out, _, _) = shim(&s_, &["plugin", "list"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "1 plugin installed:", "{out}");
    assert_eq!(
        lines[1],
        format!("- acme.rest (Rest) enabled [local:{}]", src.display()),
        "{out}"
    );
    assert!(lines[2].starts_with("  config: "), "{out}");
    let (out, _, _) = shim(&s_, &["plugin", "list", "--json", "--plugin", "acme.rest"]);
    let l: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(l["result"]["type"], "plugin_list", "{l}");
    assert_eq!(l["result"]["plugins"].as_array().unwrap().len(), 1, "{l}");
    let (out, _, _) = shim(&s_, &["plugin", "list", "--json", "--plugin", "acme.none"]);
    let l: Value = serde_json::from_str(out.trim()).unwrap();
    assert!(l["result"]["plugins"].as_array().unwrap().is_empty(), "{l}");

    // `plugin action list`: Herdr's `PluginActionInfo`, sorted, with the manifest command.
    let acts = s_.herdr(&["plugin", "action", "list"]);
    let a = acts["actions"].as_array().unwrap();
    assert_eq!(a.len(), 2, "{acts}");
    assert_eq!(a[0]["action_id"], "alpha", "{acts}");
    assert_eq!(a[0]["plugin_id"], "acme.rest", "{acts}");
    assert_eq!(a[0]["title"], "Alpha", "{acts}");
    assert_eq!(a[0]["description"], "First", "{acts}");
    assert_eq!(a[0]["command"], json!(["sh", "-c", "echo alpha"]), "{acts}");
    assert_eq!(a[0]["platforms"], json!(["linux", "macos"]), "{acts}");
    assert_eq!(a[1]["action_id"], "zeta", "{acts}");
    assert!(a[1].get("contexts").is_none(), "{acts}");
    let acts = s_.herdr(&["plugin", "action", "list", "--plugin", "acme.none"]);
    assert!(acts["actions"].as_array().unwrap().is_empty(), "{acts}");

    // `config-dir` prints the bare path and creates it.
    let (out, _, code) = shim(&s_, &["plugin", "config-dir", "acme.rest"]);
    assert_eq!(code, 0);
    let dir = out.trim_end_matches('\n');
    assert_eq!(out.lines().count(), 1, "{out}");
    assert!(Path::new(dir).is_dir(), "{out}");

    // Disable, enable, unlink: response lines.
    let (out, _, _) = shim(&s_, &["plugin", "disable", "acme.rest"]);
    let r: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(r["result"]["type"], "plugin_disabled", "{r}");
    assert_eq!(r["result"]["plugin"]["enabled"], false, "{r}");
    let (out, _, _) = shim(&s_, &["plugin", "enable", "acme.rest"]);
    let r: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(r["result"]["type"], "plugin_enabled", "{r}");
    let (out, _, _) = shim(&s_, &["plugin", "unlink", "acme.rest"]);
    let r: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        r["result"],
        json!({"type": "plugin_unlinked", "plugin_id": "acme.rest", "removed": true}),
        "{r}"
    );
    let (out, _, _) = shim(&s_, &["plugin", "unlink", "acme.rest"]);
    let r: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(r["result"]["removed"], false, "{r}");
    assert!(src.join("herdr-plugin.toml").exists(), "unlink keeps files");

    // Errors are Herdr error lines (exit 1); uninstall prints Herdr's text.
    let (_, err, code) = shim(&s_, &["plugin", "enable", "acme.rest"]);
    assert_eq!(code, 1, "{err}");
    let e: Value = serde_json::from_str(err.trim()).unwrap();
    assert_eq!(e["id"], "cli:plugin", "{e}");
    assert!(e["error"]["code"].is_string(), "{e}");
    let (_, err, code) = shim(&s_, &["plugin", "uninstall", "acme.none"]);
    assert_eq!((err.trim(), code), ("plugin not installed: acme.none", 1));
    shim(
        &s_,
        &["plugin", "link", src.to_str().unwrap(), "--disabled"],
    );
    let (out, _, code) = shim(&s_, &["plugin", "list"]);
    assert_eq!(code, 0);
    assert!(out.contains("- acme.rest (Rest) disabled ["), "{out}");
    let (out, _, code) = shim(&s_, &["plugin", "uninstall", "acme.rest"]);
    assert_eq!((out.as_str(), code), ("Uninstalled acme.rest.\n", 0));
    assert!(
        src.join("herdr-plugin.toml").exists(),
        "a linked plugin keeps its files"
    );
}
