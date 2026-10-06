//! M4 parity polish end to end via the CLI against isolated sessions (11 §M4): layout
//! export/apply and named layouts, archived-scrollback search with read scope, native
//! notifications (log fake) with click-to-focus, theme propagation, groups, floating panes,
//! status segments and jj task workspaces (fake `jj`).

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new(config: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkm4")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        Session { dir }
    }
    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().canonicalize().unwrap().join(p)
    }
    fn notes(&self) -> PathBuf {
        self.path("notes.jsonl")
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            // Never pop real OS notifications from tests: the log fake records them.
            .env(
                "VIBEKE_NOTIFIER",
                format!("log:{}", d.join("notes.jsonl").display()),
            )
            .env("VIBEKE_JJ", d.join("fakejj/jj"));
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
    /// `(exit code, stderr json)` for a call expected to fail.
    fn fail(&self, args: &[&str]) -> (i32, Value) {
        let out = self.cmd(args).output().unwrap();
        assert!(!out.status.success(), "{args:?} should fail");
        (
            out.status.code().unwrap_or(-1),
            serde_json::from_slice(&out.stderr).unwrap_or(Value::Null),
        )
    }
    fn wait_output(&self, pane: &str, pat: &str, timeout_ms: u64) -> bool {
        self.cmd(&[
            "pane",
            "wait-output",
            pane,
            "--regex",
            pat,
            "--timeout-ms",
            &timeout_ms.to_string(),
        ])
        .output()
        .unwrap()
        .status
        .success()
    }
    fn settle(&self, pane: &str) {
        let _ = self
            .cmd(&[
                "pane",
                "wait-idle",
                pane,
                "--quiet-ms",
                "600",
                "--timeout-ms",
                "10000",
            ])
            .output();
    }
    /// Run a shell line inside `pane` and return its recent output.
    fn in_pane(&self, pane: &str, line: &str) -> String {
        let marker = format!("done-{}", tag());
        let _ = self.json(&["pane", "run", pane, &format!("{line}; echo; echo {marker}")]);
        assert!(
            self.wait_output(pane, &format!("(?m)^{marker}$"), 15000),
            "marker not seen in {pane}"
        );
        std::thread::sleep(Duration::from_millis(150));
        self.json(&["pane", "read", pane, "--source", "recent", "--lines", "60"])["text"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
    fn tab(&self, id: &str) -> Value {
        self.json(&["tab", "list"])["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn tag() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
}

fn wait_for(mut f: impl FnMut() -> bool, ms: u64) -> bool {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

#[test]
fn layouts_export_apply_and_named() {
    let probe = Session::new("");
    let proj = probe.path("proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    let cfg = format!(
        r#"
[layouts.dev]
cwd = "{}"
description = "editor, watcher and a float"
[[layouts.dev.tab]]
title = "edit"
[layouts.dev.tab.pane]
split = "right"
[[layouts.dev.tab.pane.children]]
size = 0.7
run = "echo hello-from-$((40+2))"
[[layouts.dev.tab.pane.children]]
cwd = "src"
command = ["sleep", "600"]
[[layouts.dev.tab.float]]
command = "sleep 599"
rect = {{ x = 10, y = 10, w = 80, h = 80 }}
[[layouts.dev.tab]]
title = "logs"
pane = {{ split = "down", children = [{{}}, {{ size = 0.25 }}] }}
"#,
        proj.display()
    );
    std::fs::write(probe.path("config.toml"), cfg).unwrap();
    let s_ = &probe;

    let l = s_.json(&["layout", "list"]);
    assert_eq!(l["layouts"][0]["name"], "dev");
    assert_eq!(l["layouts"][0]["panes"], 5);

    let r = s_.json(&["workspace", "create", "--layout", "dev", "--name", "devws"]);
    assert_eq!(r["workspace"]["name"], "devws");
    assert_eq!(s(&r["workspace"]["root_path"]), proj.to_string_lossy());
    let tabs = r["tabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs[0]["title"], "edit");
    let split = &tabs[0]["layout"]["Split"];
    assert_eq!(split["dir"], "Horizontal");
    assert!((split["children"][0][1].as_f64().unwrap() - 0.7).abs() < 0.01);
    assert_eq!(tabs[0]["floating"].as_array().unwrap().len(), 1);
    assert_eq!(tabs[1]["layout"]["Split"]["dir"], "Vertical");
    let panes = r["panes"].as_array().unwrap();
    assert_eq!(panes.len(), 5);
    let editor = s(&split["children"][0][0]["Leaf"]["pane"]);
    let watcher = s(&split["children"][1][0]["Leaf"]["pane"]);
    let wpane = panes.iter().find(|p| p["id"] == watcher.as_str()).unwrap();
    assert_eq!(s(&wpane["cwd"]), proj.join("src").to_string_lossy());
    // `run` lines are typed into the shell once it is up.
    assert!(
        s_.wait_output(&editor, "hello-from-42", 15000),
        "run line not delivered"
    );

    // Export → TOML → apply as a file into a new workspace: same shape.
    std::thread::sleep(Duration::from_millis(1500)); // process info refresh
    let ex = s_.json(&[
        "layout",
        "export",
        "--workspace",
        "devws",
        "--format",
        "toml",
    ]);
    let toml_text = s(&ex["toml"]);
    assert!(toml_text.contains("split = \"right\""), "{toml_text}");
    assert!(toml_text.contains("sleep"), "{toml_text}");
    assert!(toml_text.contains("[[tab.float]]"), "{toml_text}");
    let file = probe.path("exported.toml");
    std::fs::write(&file, &toml_text).unwrap();
    let r2 = s_.json(&[
        "layout",
        "apply",
        &file.to_string_lossy(),
        "--ws-name",
        "copy",
    ]);
    assert_eq!(r2["workspace"]["name"], "copy");
    assert_eq!(r2["tabs"].as_array().unwrap().len(), 2);
    assert_eq!(r2["panes"].as_array().unwrap().len(), 5);
    let t0 = &r2["tabs"][0];
    assert_eq!(
        t0["layout"]["Split"]["children"].as_array().unwrap().len(),
        2
    );
    assert_eq!(t0["floating"][0]["w"], 80.0);

    // A single tab exported as JSON applies into an existing workspace.
    let ex_tab = s_.json(&["layout", "export", &s(&tabs[1]["id"])]);
    assert_eq!(ex_tab["scope"], "tab");
    let doc = serde_json::to_string(&ex_tab).unwrap();
    let r3 = s_.json(&["layout", "apply", "--doc", &doc, "--workspace", "devws"]);
    assert_eq!(r3["workspace"]["name"], "devws");
    assert_eq!(r3["tabs"][0]["number"], 3);

    // Errors: unknown name, malformed doc.
    let (code, e) = s_.fail(&["layout", "apply", "nosuch"]);
    assert_eq!(code, 1);
    assert_eq!(e["error"]["kind"], "not_found");
    let (_, e) = s_.fail(&["layout", "apply", "--doc", "[[tab]]\nbogus = 1"]);
    assert_eq!(e["error"]["kind"], "invalid_params");
}

#[test]
fn search_archive_paging_and_read_scope() {
    let s_ = Session::new("");
    let a = s(&s_.json(&["workspace", "create", "--cwd", "/tmp"])["root_pane"]["id"]);
    let b = s(
        &s_.json(&["workspace", "create", "--cwd", "/tmp", "--name", "other"])["root_pane"]["id"],
    );
    s_.settle(&a);
    s_.settle(&b);
    // More lines than the in-memory scrollback: the oldest live only in the archive.
    s_.json(&["pane", "run", &a, "seq -f 'needle-%g zebra' 1 12000"]);
    assert!(s_.wait_output(&a, "(?m)^needle-12000 zebra$", 30000));
    // Archive + FTS flush at 1 Hz.
    assert!(wait_for(
        || {
            s_.json(&["search", "needle-7", "--pane", &a, "--sources", "archive"])["hits"]
                .as_array()
                .is_some_and(|h| !h.is_empty())
        },
        8000
    ));
    let r = s_.json(&[
        "search",
        "needle-7",
        "--pane",
        &a,
        "--sources",
        "archive",
        "--context",
        "1",
    ]);
    let hit = &r["hits"][0];
    assert_eq!(hit["source"], "archive");
    assert_eq!(hit["text"], "needle-7 zebra");
    assert_eq!(hit["context"]["before"][0], "needle-6 zebra");
    assert_eq!(hit["context"]["after"][0], "needle-8 zebra");
    let line = hit["line"].as_u64().unwrap();
    let from = hit["position"]["from"].as_u64().unwrap();
    let to = hit["position"]["to"].as_u64().unwrap();
    assert_eq!((from, to), (line - 1, line + 2));

    // Page the archive around the hit (edit-scrollback / copy mode paging).
    let page = s_.json(&[
        "pane",
        "read",
        &a,
        "--source",
        "archive",
        "--from",
        &from.to_string(),
        "--to",
        &to.to_string(),
    ]);
    assert_eq!(
        page["text"],
        "needle-6 zebra\nneedle-7 zebra\nneedle-8 zebra"
    );
    assert_eq!(page["more_before"], true);
    // The tail of the same history comes from memory and the screen.
    let tail = s_.json(&["pane", "read", &a, "--source", "archive", "--lines", "5"]);
    assert!(s(&tail["text"]).contains("needle-12000 zebra"), "{tail}");
    assert_eq!(tail["more_after"], false);

    // Live hits (screen/scrollback) with a regex, newest first.
    let r = s_.json(&[
        "search",
        "needle-1199[0-9] zebra$",
        "--regex",
        "--limit",
        "3",
    ]);
    let hits = r["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0]["text"], "needle-11999 zebra");
    assert_eq!(r["truncated"], true);
    // `since` in the future excludes archive rows.
    let r = s_.json(&[
        "search",
        "needle-7",
        "--sources",
        "archive",
        "--since",
        "99999999999999",
    ]);
    assert_eq!(r["hits"].as_array().unwrap().len(), 0);

    // Read scope: a pane in another workspace sees nothing of A.
    let out = s_.in_pane(
        &b,
        "$VIBEKE_BIN --json search needle-7 --sources archive 2>&1 | head -c 120",
    );
    assert!(out.contains("\"hits\":[]"), "{out}");
    let out = s_.in_pane(
        &b,
        &format!("$VIBEKE_BIN --json pane read {a} --source archive 2>&1 | head -c 160"),
    );
    assert!(out.contains("permission_denied"), "{out}");
    let out = s_.in_pane(
        &b,
        &format!("$VIBEKE_BIN --json pane read {a} --source recent 2>&1 | head -c 160"),
    );
    assert!(out.contains("permission_denied"), "{out}");
    // …but its own workspace is readable.
    let out = s_.in_pane(
        &b,
        "$VIBEKE_BIN --json pane read --current --source archive --lines 3 2>&1 | head -c 120",
    );
    assert!(out.contains("\"source\":\"archive\""), "{out}");
}

#[test]
fn notifications_focus_theme_and_status() {
    let s_ = Session::new("");
    let r = s_.json(&["workspace", "create", "--cwd", "/tmp", "--name", "nw"]);
    let p = s(&r["root_pane"]["id"]);
    s_.settle(&p);

    let n = s_.json(&["notify", "--pane", &p, "build done", "all green"]);
    let ch: Vec<String> = n["notification"]["channels"]
        .as_array()
        .unwrap()
        .iter()
        .map(s)
        .collect();
    assert!(ch.contains(&"native".to_string()), "{ch:?}");
    // Same pane within coalesce_ms: merged.
    let n2 = s_.json(&["notify", "--pane", &p, "build done again"]);
    assert!(
        n2["notification"]["channels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "coalesced")
    );
    let notes = std::fs::read_to_string(s_.notes()).unwrap();
    let lines: Vec<Value> = notes
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|v: &Value| v["event"] == "notify")
        .collect();
    assert_eq!(lines.len(), 1, "{notes}");
    let url = s(&lines[0]["note"]["url"]);
    assert_eq!(url, format!("vibeke://focus?session=default&pane={p}"));
    assert_eq!(lines[0]["note"]["on_click"][3], "focus");
    let cfg = s_.json(&["notification", "config"]);
    assert_eq!(cfg["native"]["backend"], "log");
    assert_eq!(cfg["channels"][1], "native");

    // Click-to-focus: `vibeke focus <url>` resolves the pane and raises the host terminal.
    let f = s_.json(&["focus", &url]);
    assert_eq!(f["pane"]["id"], p.as_str());
    assert_eq!(f["raised"], true);
    assert!(
        std::fs::read_to_string(s_.notes())
            .unwrap()
            .contains("\"event\":\"raise\"")
    );
    let (_, e) = s_.fail(&["focus", "vibeke://focus?session=default&pane=nope"]);
    assert_eq!(e["error"]["kind"], "not_found");

    // Theme: a light host → theme.changed, model appearance, COLORFGBG in new panes.
    let a = s_.json(&["client", "appearance", "--dark", "false"]);
    assert_eq!(a["appearance"]["theme"], "catppuccin-latte");
    let ev = s_.json(&["events", "read", "--types", "theme.changed"]);
    assert_eq!(ev["events"].as_array().unwrap().len(), 1);
    assert_eq!(ev["events"][0]["data"]["dark"], false);
    let snap = s_.json(&["session", "snapshot"]);
    assert_eq!(snap["appearance"]["dark"], false);
    let q = s(&s_.json(&["pane", "split", &p, "--direction", "down"])["pane"]["id"]);
    s_.settle(&q);
    let out = s_.in_pane(&q, "echo fgbg=$COLORFGBG theme=$VIBEKE_THEME");
    assert!(out.contains("fgbg=0;15 theme=light"), "{out}");
    let m = s_.json(&["theme", "set-mode", "dark"]);
    assert_eq!(m["appearance"]["dark"], true);

    // Status segment data from the pane's point of view.
    let st = s_.json(&["status", "segments", "--pane", &p]);
    assert_eq!(st["segments"]["workspace"]["name"], "nw");
    assert_eq!(st["segments"]["session"]["name"], "default");
    assert!(st["segments"]["clock"]["ms"].as_i64().unwrap() > 0);
    assert_eq!(st["client_side"][0], "mode");
}

#[test]
fn groups_and_floating_panes() {
    let s_ = Session::new("");
    let g = s_.json(&["group", "create", "clients"]);
    assert_eq!(g["group"]["handle"], "g1");
    let r = s_.json(&["workspace", "create", "--cwd", "/tmp", "--group", "clients"]);
    let ws = s(&r["workspace"]["id"]);
    assert_eq!(r["group"], g["group"]["id"]);
    s_.json(&["workspace", "create", "--cwd", "/tmp", "--name", "loose"]);
    let in_g = s_.json(&["workspace", "list", "--group", "clients"]);
    assert_eq!(in_g["workspaces"].as_array().unwrap().len(), 1);
    let all = s_.json(&["workspace", "list"]);
    assert_eq!(all["workspaces"].as_array().unwrap().len(), 2);
    assert!(
        all["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["group"] == g["group"]["id"])
    );
    s_.json(&["group", "collapse", "clients"]);
    let gl = s_.json(&["group", "list"]);
    assert_eq!(gl["groups"][0]["collapsed"], true);
    assert_eq!(gl["groups"][0]["workspaces"][0], ws.as_str());
    s_.json(&["workspace", "move", &ws, "--group", ""]);
    let gl = s_.json(&["group", "list"]);
    assert_eq!(gl["groups"][0]["workspaces"].as_array().unwrap().len(), 0);
    assert_eq!(gl["ungrouped"].as_array().unwrap().len(), 2);
    s_.json(&["group", "delete", "clients"]);
    assert_eq!(s_.json(&["group", "list"])["groups"], serde_json::json!([]));

    // Floating panes.
    let p1 = s(&r["root_pane"]["id"]);
    let tab = s(&r["tab"]["id"]);
    let f = s_.json(&["pane", "float", "--tab", &tab, "--command", "sleep 300"]);
    let fl = s(&f["pane"]["id"]);
    assert_eq!(f["tab"]["floating"][0]["pane"], fl.as_str());
    assert_eq!(f["tab"]["floating"][0]["w"], 70.0);
    let p2 = s(&s_.json(&["pane", "split", &p1, "--direction", "right"])["pane"]["id"]);
    // Float a tiled pane with a geometry, then move it back into the tiling.
    let rect = r#"{"x":5,"y":5,"w":50,"h":40}"#;
    let t = s_.json(&["pane", "float", &p2, "--rect", rect])["tab"].clone();
    assert_eq!(t["floating"].as_array().unwrap().len(), 2);
    assert_eq!(t["layout"]["Leaf"]["pane"], p1.as_str());
    assert_eq!(t["floating"][1]["h"], 40.0);
    assert!(t["floating"][1]["z"].as_u64() > t["floating"][0]["z"].as_u64());
    let (_, e) = s_.fail(&["pane", "float", &p1]);
    assert_eq!(e["error"]["kind"], "conflict"); // last tiled pane
    let t = s_.json(&["pane", "embed", &p2, "--direction", "down"])["tab"].clone();
    assert_eq!(t["floating"].as_array().unwrap().len(), 1);
    assert_eq!(t["layout"]["Split"]["dir"], "Vertical");
    let t = s_.json(&["tab", "floats", &tab])["tab"].clone();
    assert_eq!(t["floats_hidden"], true);
    // Exported layouts keep floats.
    let ex = s_.json(&["layout", "export", &tab]);
    assert_eq!(ex["layout"]["tab"][0]["float"][0]["rect"]["w"], 70.0);
    // Closing a float removes it from the tab.
    s_.json(&["pane", "close", &fl]);
    assert!(wait_for(
        || s_.tab(&tab)["floating"]
            .as_array()
            .is_some_and(|a| a.is_empty()),
        8000
    ));
    assert!(!s_.tab(&tab).is_null());
}

fn fake_jj(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let log = dir.join("calls.log");
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "jj 0.99.0-fake"; exit 0; fi
echo "$@" >> '{log}'
shift 4
case "$1 $2" in
  "workspace add") mkdir -p "$7"; echo "Created workspace" ;;
  "workspace list") echo "default: aaa 111 (empty) (no description set)" ;;
  "workspace forget") ;;
  "log --no-graph")
     case "$*" in
       *trunk*) echo "deadbeef" ;;
       *) printf 'kxyz\tab12\t\tchanged\tok\twip\nqpar\tcd34\tme/fix-login\tempty\tok\t\n' ;;
     esac ;;
  *) echo "unexpected: $*" >&2; exit 1 ;;
esac
"#,
        log = log.display()
    );
    let bin = dir.join("jj");
    std::fs::write(&bin, script).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn jj_workspace_tasks_with_fake_jj() {
    let s_ = Session::new("[tasks]\nbranch_template = \"me/{slug}\"\n");
    fake_jj(&s_.path("fakejj"));
    let repo = s_.path("repo");
    std::fs::create_dir_all(repo.join(".jj")).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let wt = s_.path("wt");
    let r = s_.json(&[
        "task",
        "new",
        "Fix login",
        "--repo",
        &repo.to_string_lossy(),
        "--isolation",
        "jj_workspace",
        "--root",
        &wt.to_string_lossy(),
        "--branch-template",
        "me/{slug}",
    ]);
    let task = &r["task"];
    assert_eq!(task["checkout"], "jj_workspace");
    assert_eq!(task["branch"], "me/fix-login");
    assert_eq!(task["base_ref"], "trunk()");
    let path = PathBuf::from(s(&task["worktree_path"]));
    assert!(path.is_dir(), "{path:?}");
    assert!(path.starts_with(&wt));
    let calls = std::fs::read_to_string(s_.path("fakejj/calls.log")).unwrap();
    assert!(
        calls.contains("workspace add --name fix-login -r trunk()"),
        "{calls}"
    );
    // The workspace's pane opens in the jj workspace.
    assert_eq!(s(&r["panes"][0]["cwd"]), path.to_string_lossy());

    let g = s_.json(&["task", "get", &s(&task["id"])]);
    assert_eq!(g["branch_status"]["vcs"], "jj");
    assert_eq!(g["branch_status"]["bookmark_exists"], true);
    assert_eq!(g["branch_status"]["dirty"], true);
    assert_eq!(g["jj"]["colocated"], true);

    s_.json(&["task", "finish", &s(&task["id"]), "--remove-worktree"]);
    assert!(
        wait_for(|| !path.exists(), 8000),
        "workspace dir not removed"
    );
    let calls = std::fs::read_to_string(s_.path("fakejj/calls.log")).unwrap();
    assert!(calls.contains("workspace forget fix-login"), "{calls}");

    // Not a jj repo → refused before anything is created.
    let (_, e) = s_.fail(&[
        "task",
        "new",
        "Nope",
        "--repo",
        "/tmp",
        "--isolation",
        "jj_workspace",
    ]);
    assert_eq!(e["error"]["kind"], "invalid_params");
}
