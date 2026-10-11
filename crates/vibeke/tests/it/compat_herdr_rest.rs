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
        "invalid_request"
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
    let r = call(&s_, "pane.copy_motion", json!({"pane_id": p1}));
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
    // One reorder event for the whole block (Herdr's `workspace.reordered`), carried by one
    // native `workspace.moved` with the block.
    let moved = s_.events("workspace.moved");
    let blocks: Vec<&Value> = moved
        .iter()
        .filter(|e| e["data"].get("workspace_ids").is_some())
        .collect();
    assert_eq!(blocks.len(), 1, "{moved:?}");
    assert_eq!(blocks[0]["data"]["workspace_ids"], json!([w2]), "{moved:?}");
    assert_eq!(
        blocks[0]["data"]["before_workspace_id"],
        ws.as_str(),
        "{moved:?}"
    );
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
        json!({"path": checkout.to_str().unwrap(), "cwd": main.to_str().unwrap()}),
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
        json!({"path": dirty.to_str().unwrap(), "cwd": main.to_str().unwrap()}),
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

#[test]
fn herdr_rest_worktree_open_and_create_take_herdrs_params() {
    let s_ = session("");
    write(
        &s_.path("config.toml"),
        &format!(
            "[tasks]\nroot = \"{}\"\nfetch_before_create = false\n",
            s_.path("wt").display()
        ),
    );
    let main = s_.path("app");
    repo(&main);
    let m = main.to_str().unwrap();
    let feat = s_.path("app-feat");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            feat.to_str().unwrap(),
        ],
    );

    // `worktree.open` by branch from `cwd`; `label` names the new workspace. The main
    // checkout gets its parent workspace first, as in Herdr.
    let o = ok(
        &s_,
        "worktree.open",
        json!({"cwd": m, "branch": "feat", "label": "Feature"}),
    );
    assert_eq!(o["type"], "worktree_opened", "{o}");
    assert_eq!(o["already_open"], false, "{o}");
    assert_eq!(o["worktree"]["path"], feat.to_str().unwrap(), "{o}");
    assert_eq!(o["worktree"]["branch"], "feat", "{o}");
    assert_eq!(o["worktree"]["is_linked_worktree"], true, "{o}");
    assert_eq!(o["worktree"]["label"], "app", "{o}");
    assert_eq!(o["workspace"]["label"], "Feature", "{o}");
    let ws = s(&o["workspace"]["workspace_id"]);
    assert_eq!(o["worktree"]["open_workspace_id"], ws.as_str(), "{o}");
    assert_eq!(o["tab"]["workspace_id"], ws.as_str(), "{o}");
    assert!(o["root_pane"]["pane_id"].is_string(), "{o}");
    let l = ok(&s_, "worktree.list", json!({"cwd": m}));
    let parent = s(&l["source"]["source_workspace_id"]);
    assert!(!parent.is_empty() && parent != ws, "{l}");
    // Again: the same workspace, `already_open`.
    let o = ok(
        &s_,
        "worktree.open",
        json!({"workspace_id": parent, "path": feat.to_str().unwrap()}),
    );
    assert_eq!(o["already_open"], true, "{o}");
    assert_eq!(o["workspace"]["workspace_id"], ws.as_str(), "{o}");

    // Herdr's errors: exactly one of path/branch, an unknown branch, a linked source, a
    // relative path.
    for (params, code) in [
        (json!({"cwd": m}), "invalid_request"),
        (
            json!({"cwd": m, "path": "/x", "branch": "feat"}),
            "invalid_request",
        ),
        (json!({"cwd": m, "branch": "nope"}), "worktree_not_found"),
        (json!({"cwd": m, "path": "/nope"}), "worktree_not_found"),
        (
            json!({"cwd": feat.to_str().unwrap(), "branch": "feat"}),
            "linked_worktree_source",
        ),
        (json!({"cwd": "app", "branch": "feat"}), "invalid_request"),
    ] {
        assert_eq!(
            err_code(&s_, "worktree.open", params.clone()),
            code,
            "{params}"
        );
    }

    // `worktree.create` with an explicit `path`, `base` and `label`.
    let dest = s_.path("elsewhere/app-two");
    let c = ok(
        &s_,
        "worktree.create",
        json!({"cwd": m, "branch": "two", "base": "main", "path": dest.to_str().unwrap(), "label": "Two"}),
    );
    assert_eq!(c["type"], "worktree_created", "{c}");
    assert_eq!(c["worktree"]["path"], dest.to_str().unwrap(), "{c}");
    assert_eq!(c["worktree"]["branch"], "two", "{c}");
    assert_eq!(c["worktree"]["is_linked_worktree"], true, "{c}");
    assert_eq!(c["workspace"]["label"], "Two", "{c}");
    assert!(dest.join("README.md").exists(), "{c}");
    assert!(
        c["tab"]["tab_id"].is_string() && c["root_pane"]["pane_id"].is_string(),
        "{c}"
    );
    // An existing local branch is checked out rather than created.
    git(&main, &["branch", "three"]);
    let c = ok(
        &s_,
        "worktree.create",
        json!({"cwd": m, "branch": "three", "path": s_.path("app-three").to_str().unwrap()}),
    );
    assert_eq!(c["worktree"]["branch"], "three", "{c}");
    // Without `branch`, Herdr's generated `worktree/<adjective>-<noun>-<hex>`, under the
    // tasks root.
    let c = ok(&s_, "worktree.create", json!({"cwd": m}));
    let b = s(&c["worktree"]["branch"]);
    assert!(b.starts_with("worktree/"), "{c}");
    assert!(
        s(&c["worktree"]["path"]).starts_with(s_.path("wt/app").to_str().unwrap()),
        "{c}"
    );
    assert_eq!(
        err_code(&s_, "worktree.create", json!({"cwd": m, "branch": "  "})),
        "invalid_request"
    );
    assert_eq!(
        err_code(
            &s_,
            "worktree.create",
            json!({"cwd": m, "branch": "x", "path": "rel/path"})
        ),
        "invalid_request"
    );
    assert_eq!(
        err_code(
            &s_,
            "worktree.create",
            json!({"cwd": m, "branch": "two", "path": dest.to_str().unwrap()})
        ),
        "worktree_create_failed"
    );

    // `worktree.list` reports bare entries as Herdr does, and `trust_repository` is accepted.
    let l = ok(
        &s_,
        "worktree.list",
        json!({"cwd": m, "trust_repository": true}),
    );
    let wts = l["worktrees"].as_array().unwrap();
    assert!(wts.iter().all(|e| e["is_bare"] == false), "{l}");
    assert_eq!(wts.len(), 5, "{l}");
}

/// Echo `$name` in a pane and wait for `want` (Herdr's `pane.wait_for_output`).
fn expect_env(s_: &Session, pane: &str, name: &str, want: &str) {
    s_.herdr(&[
        "pane",
        "send-text",
        pane,
        &format!("echo \"env-$((1+1))-${name}\"\n"),
    ]);
    let r = ok(
        s_,
        "pane.wait_for_output",
        json!({"pane_id": pane, "source": "recent", "match": {"type": "substring", "value": format!("env-2-{want}")}, "timeout_ms": 20000}),
    );
    assert_eq!(r["type"], "output_matched", "{r}");
}

#[test]
fn herdr_rest_launch_env_for_split_tab_and_workspace() {
    let s_ = session("");
    let w = ok(
        &s_,
        "workspace.create",
        json!({"cwd": "/tmp", "env": {"HP5_WS": "ws-value"}}),
    );
    let ws = s(&w["workspace"]["workspace_id"]);
    let root = s(&w["root_pane"]["pane_id"]);
    expect_env(&s_, &root, "HP5_WS", "ws-value");

    // `pane.split` with `env`; the pane's identity cannot be replaced.
    let sp_ = ok(
        &s_,
        "pane.split",
        json!({"target_pane_id": root, "direction": "right", "env": {"HP5_SPLIT": "split-value", "VIBEKE_PANE_ID": "forged", "HERDR_PANE_ID": "forged"}, "right_click": "pane"}),
    );
    let split = s(&sp_["pane"]["pane_id"]);
    expect_env(&s_, &split, "HP5_SPLIT", "split-value");
    expect_env(&s_, &split, "VIBEKE_PANE_ID", &split);

    // `tab.create` with `env`.
    let t = ok(
        &s_,
        "tab.create",
        json!({"workspace_id": ws, "env": {"HP5_TAB": "tab-value"}}),
    );
    expect_env(&s_, &s(&t["root_pane"]["pane_id"]), "HP5_TAB", "tab-value");

    // Herdr's `normalize_launch_env` errors and the `right_click` enum.
    assert_eq!(
        err_code(
            &s_,
            "pane.split",
            json!({"target_pane_id": root, "direction": "down", "env": {"A=B": "x"}})
        ),
        "invalid_env"
    );
    assert_eq!(
        err_code(&s_, "tab.create", json!({"env": {"": "x"}})),
        "invalid_env"
    );
    assert_eq!(
        err_code(
            &s_,
            "pane.split",
            json!({"target_pane_id": root, "direction": "down", "right_click": "menu"})
        ),
        "invalid_request"
    );
    assert_eq!(
        err_code(
            &s_,
            "pane.split",
            json!({"target_pane_id": "w99:p1", "direction": "down"})
        ),
        "pane_not_found"
    );
    // `workspace_id` splits that workspace's focused pane.
    let sp2 = ok(
        &s_,
        "pane.split",
        json!({"workspace_id": ws, "direction": "down"}),
    );
    assert_eq!(sp2["pane"]["workspace_id"], ws.as_str(), "{sp2}");
    // The native API takes `env` too.
    let n = s_.api(
        "workspace.create",
        json!({"cwd": "/tmp", "env": {"HP5_NATIVE": "native-value"}}),
    );
    let np = s(&n["root_pane"]["handle"]);
    expect_env(&s_, &np, "HP5_NATIVE", "native-value");
    let bad = s_
        .cmd(&[
            "api",
            "call",
            "pane.split",
            &json!({"pane": np, "env": {"K": 1}}).to_string(),
        ])
        .output()
        .unwrap();
    assert!(!bad.status.success(), "a non-string env value is refused");
}

#[test]
fn herdr_rest_pane_links_resolve_and_activate_like_a_click() {
    let s_ = session("");
    let src = s_.path("src/links");
    write(
        &src.join("herdr-plugin.toml"),
        r#"id = "acme.links"
name = "Links"
version = "0.1.0"

[[actions]]
id = "open"
title = "Open"
command = ["sh", "-c", "printf '%s|%s|%s' \"$HERDR_PLUGIN_CLICKED_URL\" \"$HERDR_PLUGIN_LINK_HANDLER_ID\" \"$HERDR_PLUGIN_CONTEXT_JSON\" > \"$HERDR_PLUGIN_STATE_DIR/link.txt\""]

[[link_handlers]]
id = "example"
title = "Example"
pattern = '^https://example\.com/'
action = "open"
"#,
    );
    s_.json(&["plugin", "link", src.to_str().unwrap()]);
    s_.json(&["plugin", "trust", "acme.links", "--legacy"]);
    let created = ok(
        &s_,
        "workspace.create",
        json!({"cwd": "/tmp", "focus": true}),
    );
    let ws = s(&created["workspace"]["workspace_id"]);
    let pane = s(&created["root_pane"]["pane_id"]);
    ok(&s_, "workspace.focus", json!({"workspace_id": ws}));
    s_.herdr(&[
        "pane",
        "send-text",
        &pane,
        "printf 'go: https://example.com/x/%s, done\\n' $((40+2))\n",
    ]);
    ok(
        &s_,
        "pane.wait_for_output",
        json!({"pane_id": pane, "source": "visible", "match": {"type": "substring", "value": "go: https://example.com/x/42"}, "timeout_ms": 20000}),
    );
    let read = ok(
        &s_,
        "pane.read",
        json!({"pane_id": pane, "source": "visible"}),
    );
    let text = s(&read["read"]["text"]);
    let (row, line) = text
        .lines()
        .enumerate()
        .find(|(_, l)| l.starts_with("go: https://"))
        .unwrap();
    let col = line.find("example").unwrap() as u64;

    // Resolve: the URL's cells, without the trailing comma.
    let r = ok(
        &s_,
        "pane.link.resolve",
        json!({"pane_id": pane, "viewport_row": row, "col": col}),
    );
    assert_eq!(r["type"], "pane_link_resolved", "{r}");
    assert_eq!(
        r["regions"],
        json!([{"row": row, "start_col": 4, "end_col": 4 + "https://example.com/x/42".len() - 1}]),
        "{r}"
    );
    // A blank cell has no link.
    let r = ok(
        &s_,
        "pane.link.resolve",
        json!({"pane_id": pane, "viewport_row": row, "col": 3}),
    );
    assert_eq!(r["regions"], json!([]), "{r}");

    // Activate: the handler runs like a click, with Herdr's link invocation context.
    let a = ok(
        &s_,
        "pane.link.activate",
        json!({"pane_id": pane, "viewport_row": row, "col": col, "offset_from_bottom": 0}),
    );
    assert_eq!(
        a,
        json!({"type": "pane_link_activated", "url": "https://example.com/x/42", "handled": true})
    );
    let out = s_.path("state/plugins/state/acme.links/link.txt");
    crate::support::plugins::wait_for("link handler", 15000, || out.exists());
    let got = crate::support::plugins::read(&out);
    let mut parts = got.splitn(3, '|');
    assert_eq!(parts.next(), Some("https://example.com/x/42"), "{got}");
    assert_eq!(parts.next(), Some("example"), "{got}");
    let ctx: Value = serde_json::from_str(parts.next().unwrap()).unwrap();
    assert_eq!(ctx["invocation_source"], "link_click", "{ctx}");
    assert_eq!(ctx["clicked_url"], "https://example.com/x/42", "{ctx}");
    assert_eq!(ctx["link_handler_id"], "example", "{ctx}");
    assert_eq!(ctx["focused_pane_id"], pane.as_str(), "{ctx}");

    // No handler matches a plain word; stale checks and errors as in Herdr.
    let a = ok(
        &s_,
        "pane.link.activate",
        json!({"pane_id": pane, "viewport_row": row, "col": 0}),
    );
    assert_eq!(a, json!({"type": "pane_link_activated", "handled": false}));
    assert_eq!(
        err_code(
            &s_,
            "pane.link.activate",
            json!({"pane_id": pane, "viewport_row": row, "col": col, "offset_from_bottom": 5})
        ),
        "stale_content"
    );
    assert_eq!(
        err_code(
            &s_,
            "pane.link.resolve",
            json!({"pane_id": pane, "viewport_row": row, "col": col, "content_revision": 1})
        ),
        "stale_content"
    );
    assert_eq!(
        err_code(
            &s_,
            "pane.link.resolve",
            json!({"pane_id": "w99:p1", "viewport_row": 0, "col": 0})
        ),
        "pane_not_found"
    );
    assert_eq!(
        err_code(&s_, "pane.link.resolve", json!({"pane_id": pane, "col": 0})),
        "invalid_request"
    );
}

#[test]
fn herdr_rest_clear_selection_and_agent_manifests() {
    let s_ = session("");
    let created = ok(&s_, "workspace.create", json!({"cwd": "/tmp"}));
    let pane = s(&created["root_pane"]["pane_id"]);
    s_.herdr(&[
        "pane",
        "send-text",
        &pane,
        "printf 'sel-%s one\\nsel-%s two\\n' $((1+1)) $((2+1))\n",
    ]);
    ok(
        &s_,
        "pane.wait_for_output",
        json!({"pane_id": pane, "source": "recent", "match": {"type": "substring", "value": "sel-3 two"}, "timeout_ms": 20000}),
    );

    // `pane.selection.read` over absolute rows (no scrollback yet: row = screen row).
    let read = ok(
        &s_,
        "pane.read",
        json!({"pane_id": pane, "source": "visible"}),
    );
    let text = s(&read["read"]["text"]);
    let row = text
        .lines()
        .position(|l| l.starts_with("sel-2 one"))
        .unwrap();
    let r = ok(
        &s_,
        "pane.selection.read",
        json!({"pane_id": pane, "anchor": {"row": row, "col": 4}, "cursor": {"row": row + 1, "col": 4}}),
    );
    assert_eq!(
        r,
        json!({"type": "pane_selection", "pane_id": pane, "text": "2 one\nsel-3"})
    );
    assert_eq!(
        err_code(
            &s_,
            "pane.selection.read",
            json!({"pane_id": pane, "anchor": {"row": 0, "col": 0}, "cursor": {"row": 0, "col": 1}, "content_revision": 1})
        ),
        "stale_content"
    );
    assert_eq!(
        err_code(
            &s_,
            "pane.selection.read",
            json!({"pane_id": pane, "anchor": {"row": 0, "col": 0}, "cursor": {"row": 100000, "col": 1}})
        ),
        "selection_unavailable"
    );
    assert_eq!(
        err_code(
            &s_,
            "pane.selection.read",
            json!({"pane_id": "w99:p1", "anchor": {"row": 0, "col": 0}, "cursor": {"row": 0, "col": 1}})
        ),
        "pane_not_found"
    );

    // `pane.clear`: the rows above the cursor and the scrollback are gone.
    assert_eq!(
        ok(&s_, "pane.clear", json!({"pane_id": pane})),
        json!({"type": "ok"})
    );
    let after = ok(
        &s_,
        "pane.read",
        json!({"pane_id": pane, "source": "recent", "lines": 200}),
    );
    assert!(!s(&after["read"]["text"]).contains("sel-2 one"), "{after}");
    assert_eq!(
        err_code(&s_, "pane.clear", json!({"pane_id": "w99:p1"})),
        "pane_not_found"
    );

    // Agent manifests in Herdr's `AgentManifestInfo` shape.
    let m = ok(&s_, "server.agent_manifests", json!({}));
    assert_eq!(m["type"], "agent_manifest_status", "{m}");
    let list = m["manifests"].as_array().unwrap();
    assert!(!list.is_empty(), "{m}");
    let claude = list.iter().find(|x| x["agent"] == "claude").unwrap();
    assert_eq!(claude["source"], "bundled", "{m}");
    assert_eq!(claude["source_kind"], "bundled", "{m}");
    assert_eq!(claude["local_override_shadowing_remote"], false, "{m}");
    let r = ok(&s_, "server.reload_agent_manifests", json!({}));
    assert_eq!(r["type"], "agent_manifest_reload", "{r}");
    assert_eq!(r["manifests"].as_array().unwrap().len(), list.len(), "{r}");

    // The client-only methods stay `unsupported` with their reason.
    for m in [
        "pane.copy_motion",
        "pane.copy_search",
        "pane.edit_scrollback",
        "command.invoke",
        "client_shell.surface.set",
        "integration.list",
        "server.live_handoff",
    ] {
        assert_eq!(err_code(&s_, m, json!({})), "unsupported", "{m}");
    }
}

#[test]
fn herdr_rest_release_agent_follows_the_hook_authority() {
    let s_ = session("");
    let created = ok(&s_, "workspace.create", json!({"cwd": "/tmp"}));
    let pane = s(&created["root_pane"]["pane_id"]);
    let agents = |s_: &Session| {
        ok(s_, "agent.list", json!({}))["agents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["pane_id"] == pane.as_str())
            .count()
    };
    ok(
        &s_,
        "pane.report_agent",
        json!({"pane_id": pane, "source": "hook-a", "agent": "claude", "state": "working", "seq": 5}),
    );
    crate::support::plugins::wait_for("the hook report starts a run", 15000, || agents(&s_) == 1);
    // Another source, another agent, a stale seq: the holder keeps its authority.
    for params in [
        json!({"pane_id": pane, "source": "hook-b", "agent": "claude"}),
        json!({"pane_id": pane, "source": "hook-a", "agent": "codex"}),
        json!({"pane_id": pane, "source": "hook-a", "agent": "claude", "seq": 4}),
    ] {
        assert_eq!(ok(&s_, "pane.release_agent", params.clone())["type"], "ok");
        assert_eq!(agents(&s_), 1, "{params}");
    }
    assert_eq!(
        ok(
            &s_,
            "pane.clear_agent_authority",
            json!({"pane_id": pane, "source": "hook-b"})
        )["type"],
        "ok"
    );
    assert_eq!(agents(&s_), 1, "another source cannot clear the authority");
    // The holder releases: the self-reported run ends.
    ok(
        &s_,
        "pane.release_agent",
        json!({"pane_id": pane, "source": "hook-a", "agent": "claude", "seq": 6}),
    );
    crate::support::plugins::wait_for("the holder's release ends the run", 15000, || {
        agents(&s_) == 0
    });
    // Herdr's own integration sources (`herdr:<agent>`) never release by hook. That rule is
    // not tested end to end here: Vibeke's process detection also ends an idle self-reported
    // run in a pane with no agent process, so the outcome would race.
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
    // Any id: Herdr encodes it into one directory name.
    let (out, _, code) = shim(&s_, &["plugin", "config-dir", "Acme:X/y"]);
    assert_eq!(code, 0);
    assert!(out.trim_end().ends_with("/%41cme%3A%58%2Fy"), "{out}");
    assert!(Path::new(out.trim_end()).is_dir(), "{out}");

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

#[test]
fn herdr_rest_result_shapes_follow_the_schema() {
    let s_ = session("");
    let (ws, tab, p1, p2, _) = three_panes(&s_);

    // `pane.current` answers `pane_current`; `caller_pane_id` names the pane.
    let c = ok(&s_, "pane.current", json!({"caller_pane_id": p2}));
    assert_eq!(c["type"], "pane_current", "{c}");
    assert_eq!(c["pane"]["pane_id"], p2.as_str(), "{c}");

    // `pane.read`: Herdr's `PaneReadResult`.
    s_.herdr(&["pane", "send-text", &p1, "echo rest-$((20+3))\n"]);
    let m = ok(
        &s_,
        "pane.wait_for_output",
        json!({"pane_id": p1, "source": "recent", "match": {"type": "substring", "value": "rest-23"}, "timeout_ms": 15000}),
    );
    assert_eq!(m["type"], "output_matched", "{m}");
    assert_eq!(m["pane_id"], p1.as_str(), "{m}");
    assert!(s(&m["matched_line"]).contains("rest-23"), "{m}");
    assert_eq!(m["read"]["source"], "recent_unwrapped", "{m}");
    assert_eq!(m["revision"], m["read"]["revision"], "{m}");
    let m = ok(
        &s_,
        "pane.wait_for_output",
        json!({"pane_id": p1, "source": "recent", "match": {"type": "regex", "value": "rest-2[0-9]"}, "timeout_ms": 15000}),
    );
    assert_eq!(m["type"], "output_matched", "{m}");
    assert_eq!(
        err_code(
            &s_,
            "pane.wait_for_output",
            json!({"pane_id": p1, "source": "recent", "match": {"type": "regex", "value": "("}}),
        ),
        "invalid_regex"
    );
    assert_eq!(
        err_code(
            &s_,
            "pane.wait_for_output",
            json!({"pane_id": p1, "source": "recent", "match": {"type": "substring", "value": "never-printed-xyz"}, "timeout_ms": 300}),
        ),
        "timeout"
    );
    let r = ok(
        &s_,
        "pane.read",
        json!({"pane_id": p1, "source": "recent", "lines": 20}),
    );
    assert_eq!(r["type"], "pane_read", "{r}");
    let read = &r["read"];
    assert_eq!(read["pane_id"], p1.as_str(), "{r}");
    assert_eq!(read["workspace_id"], ws.as_str(), "{r}");
    assert_eq!(read["tab_id"], tab.as_str(), "{r}");
    assert_eq!(read["source"], "recent", "{r}");
    assert_eq!(read["format"], "text", "{r}");
    // Herdr: the terminal has more rows (24 on screen) than the 20 read.
    assert_eq!(read["truncated"], true, "{r}");
    assert_eq!(read["revision"], 0, "Herdr reports revision 0: {r}");
    assert!(s(&read["text"]).contains("rest-23"), "{r}");
    // `format: ansi` keeps the colours; `visible` with `lines` reports what it left out.
    s_.herdr(&[
        "pane",
        "send-text",
        &p1,
        "printf '\\033[31mred-%s\\033[0m\\n' $((1+1))\n",
    ]);
    ok(
        &s_,
        "pane.wait_for_output",
        json!({"pane_id": p1, "source": "recent", "match": {"type": "substring", "value": "red-2"}, "timeout_ms": 20000}),
    );
    let a = ok(
        &s_,
        "pane.read",
        json!({"pane_id": p1, "source": "visible", "format": "ansi"}),
    );
    let t = s(&a["read"]["text"]);
    assert_eq!(a["read"]["format"], "ansi", "{a}");
    assert!(t.contains("\x1b[") && t.contains("red-2"), "{a}");
    assert_eq!(a["read"]["truncated"], false, "{a}");
    let plain = ok(
        &s_,
        "pane.read",
        json!({"pane_id": p1, "source": "visible", "lines": 1}),
    );
    assert!(!s(&plain["read"]["text"]).contains('\x1b'), "{plain}");
    assert_eq!(plain["read"]["truncated"], true, "{plain}");
    assert_eq!(
        err_code(
            &s_,
            "pane.read",
            json!({"pane_id": p1, "source": "visible", "format": "html"})
        ),
        "invalid_request"
    );

    // `pane.resize` and `pane.zoom` answer with their result objects and the layout.
    let r = ok(
        &s_,
        "pane.resize",
        json!({"pane_id": p1, "direction": "right", "amount": 0.1}),
    );
    assert_eq!(r["type"], "pane_resize", "{r}");
    assert_eq!(r["resize"]["pane_id"], p1.as_str(), "{r}");
    assert_eq!(r["resize"]["changed"], true, "{r}");
    assert!(r["resize"]["layout"]["panes"].is_array(), "{r}");
    let z = ok(&s_, "pane.zoom", json!({"pane_id": p1, "mode": "on"}));
    assert_eq!(z["type"], "pane_zoom", "{z}");
    assert_eq!(z["zoom"]["zoom_changed"], true, "{z}");
    assert_eq!(z["zoom"]["zoomed"], true, "{z}");
    assert_eq!(z["zoom"]["focused_pane_id"], p1.as_str(), "{z}");
    let z = ok(&s_, "pane.zoom", json!({"pane_id": p1, "mode": "on"}));
    assert_eq!(z["zoom"]["zoom_changed"], false, "{z}");
    assert_eq!(z["zoom"]["reason"], "already_zoomed", "{z}");
    let z = ok(&s_, "pane.zoom", json!({"pane_id": p1}));
    assert_eq!(z["zoom"]["zoomed"], false, "toggle unzooms: {z}");
    let z = ok(&s_, "pane.zoom", json!({"pane_id": p1, "mode": "off"}));
    assert_eq!(z["zoom"]["reason"], "already_unzoomed", "{z}");
    let single = s(&s_.herdr(&["workspace", "create", "--cwd", "/tmp"])["root_pane"]["pane_id"]);
    let z = ok(&s_, "pane.zoom", json!({"pane_id": single}));
    assert_eq!(z["zoom"]["reason"], "single_pane", "{z}");

    // Server and notification answers.
    let n = ok(
        &s_,
        "notification.show",
        json!({"title": "Rest", "body": "done"}),
    );
    assert_eq!(
        n,
        json!({"type": "notification_show", "shown": true, "reason": "shown"})
    );
    // Herdr shows one API notification per second.
    assert_eq!(
        ok(&s_, "notification.show", json!({"title": "Again"})),
        json!({"type": "notification_show", "shown": false, "reason": "rate_limited"})
    );
    assert_eq!(
        err_code(&s_, "notification.show", json!({"title": "  "})),
        "invalid_params"
    );
    let r = ok(&s_, "server.reload_config", json!({}));
    assert_eq!(
        r,
        json!({"type": "config_reload", "status": "applied", "diagnostics": []})
    );
}

#[test]
fn herdr_rest_worktree_list_shape() {
    let s_ = session("");
    let main = s_.path("repo");
    repo(&main);
    let checkout = s_.path("repo-wt");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "wt",
            checkout.to_str().unwrap(),
        ],
    );
    let w = ok(
        &s_,
        "workspace.create",
        json!({"cwd": main.to_str().unwrap()}),
    );
    let ws = s(&w["workspace"]["workspace_id"]);
    let l = ok(&s_, "worktree.list", json!({"workspace_id": ws}));
    assert_eq!(l["type"], "worktree_list", "{l}");
    let src = &l["source"];
    assert_eq!(src["repo_name"], "repo", "{l}");
    assert_eq!(src["repo_root"], main.to_str().unwrap(), "{l}");
    assert_eq!(src["source_checkout_path"], main.to_str().unwrap(), "{l}");
    assert_eq!(src["source_workspace_id"], ws.as_str(), "{l}");
    assert!(src["repo_key"].is_string(), "{l}");
    let wts = l["worktrees"].as_array().unwrap();
    assert_eq!(wts.len(), 2, "{l}");
    let main_entry = wts
        .iter()
        .find(|e| e["path"] == main.to_str().unwrap())
        .unwrap();
    assert_eq!(main_entry["is_linked_worktree"], false, "{l}");
    assert_eq!(main_entry["branch"], "main", "{l}");
    assert_eq!(main_entry["open_workspace_id"], ws.as_str(), "{l}");
    assert_eq!(main_entry["label"], "repo", "{l}");
    let linked = wts
        .iter()
        .find(|e| e["path"] == checkout.to_str().unwrap())
        .unwrap();
    assert_eq!(linked["is_linked_worktree"], true, "{l}");
    assert_eq!(linked["branch"], "wt", "{l}");
    assert_eq!(linked["is_detached"], false, "{l}");
    assert!(linked.get("open_workspace_id").is_none(), "{l}");
    // A linked checkout lists from its main checkout; `cwd` works without a workspace.
    let l2 = ok(
        &s_,
        "worktree.list",
        json!({"cwd": checkout.to_str().unwrap()}),
    );
    assert_eq!(
        l2["source"]["source_checkout_path"],
        main.to_str().unwrap(),
        "{l2}"
    );
    assert_eq!(
        err_code(
            &s_,
            "worktree.list",
            json!({"cwd": "/", "workspace_id": ws})
        ),
        "invalid_request"
    );
    assert_eq!(
        err_code(&s_, "worktree.list", json!({"cwd": "/"})),
        "not_git_worktree"
    );
}

#[test]
fn herdr_rest_output_matched_subscription_watches_the_pane() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    let s_ = session("[compat.herdr]\nenabled = true\n");
    let (_, _, p1, _, _) = three_panes(&s_);
    let sock = s_.path("run/herdr-compat/herdr.sock");
    crate::support::plugins::wait_for("compat socket", 10000, || sock.exists());
    let open = |subs: Value| {
        let mut st = UnixStream::connect(&sock).unwrap();
        st.set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        let req =
            json!({"id": "sub", "method": "events.subscribe", "params": {"subscriptions": subs}});
        st.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut rd = BufReader::new(st);
        let mut l = String::new();
        rd.read_line(&mut l).unwrap();
        (rd, serde_json::from_str::<Value>(&l).unwrap())
    };
    // A bad regex or an unknown pane refuses the subscription.
    let (_, ack) = open(
        json!([{"type": "pane.output_matched", "pane_id": p1, "source": "recent", "match": {"type": "regex", "value": "("}}]),
    );
    assert_eq!(ack["error"]["code"], "invalid_regex", "{ack}");
    let (_, ack) = open(
        json!([{"type": "pane.output_matched", "pane_id": "w99:p9", "source": "recent", "match": {"type": "substring", "value": "x"}}]),
    );
    assert_eq!(ack["error"]["code"], "pane_not_found", "{ack}");

    let (mut rd, ack) = open(
        json!([{"type": "pane.output_matched", "pane_id": p1, "source": "recent", "match": {"type": "substring", "value": "watched-42"}}]),
    );
    assert_eq!(ack["result"]["type"], "subscription_started", "{ack}");
    s_.herdr(&["pane", "send-text", &p1, "echo watched-$((40+2))\n"]);
    let mut l = String::new();
    rd.read_line(&mut l).unwrap();
    let ev: Value = serde_json::from_str(&l).unwrap();
    // Herdr's `SubscriptionEventEnvelope`: dotted name, plain data.
    assert_eq!(ev["event"], "pane.output_matched", "{ev}");
    assert_eq!(ev["data"]["pane_id"], p1.as_str(), "{ev}");
    assert!(
        s(&ev["data"]["matched_line"]).contains("watched-42"),
        "{ev}"
    );
    assert_eq!(ev["data"]["read"]["source"], "recent_unwrapped", "{ev}");
    assert!(
        s(&ev["data"]["read"]["text"]).contains("watched-42"),
        "{ev}"
    );
}
