//! Handoff destination pickers and outgoing jobs against a real server (spec 16 §15.2):
//! `fs.browse` inside `$HOME` with repositories marked and paths outside refused,
//! `repo.candidates` finding an open workspace's clone by its origin, and `handoff.cancel`
//! stopping a queued `handoff.send` job to a peer the gateway published.

mod support;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use support::{Rpc, Session};

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} in {}", dir.display());
}

/// A session whose server runs with `HOME` set to a fresh directory inside it.
struct Host {
    s: Session,
    home: PathBuf,
}

impl Host {
    fn new() -> Host {
        let s = Session::new();
        let home = s.dir.path().canonicalize().unwrap().join("home");
        std::fs::create_dir_all(&home).unwrap();
        Host { s, home }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = self.s.cmd(args);
        c.env("HOME", &self.home);
        c
    }

    fn api(&self, method: &str, p: Value) -> Result<Value, Value> {
        let out = self
            .cmd(&["api", "call", method, &p.to_string()])
            .output()
            .unwrap();
        if out.status.success() {
            Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
        } else {
            Err(serde_json::from_slice(&out.stderr).unwrap_or_else(
                |_| json!({"raw": String::from_utf8_lossy(&out.stderr).to_string()}),
            ))
        }
    }

    fn kind(e: &Value) -> String {
        e.pointer("/error/kind")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// A workspace rooted at `cwd`; returns its root pane id.
    fn workspace(&self, cwd: &Path) -> String {
        let out = self
            .cmd(&[
                "workspace",
                "create",
                "--cwd",
                cwd.to_str().unwrap(),
                "--command",
                "sleep 600",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let ws: Value = serde_json::from_slice(&out.stdout).unwrap();
        ws["root_pane"]["id"].as_str().unwrap().to_string()
    }
}

#[test]
fn browse_stays_in_home_and_marks_repositories() {
    let h = Host::new();
    let proj = h.home.join("code/proj");
    std::fs::create_dir_all(&proj).unwrap();
    git(&proj, &["init", "-q", "-b", "main"]);
    std::fs::create_dir_all(h.home.join("code/plain")).unwrap();
    std::fs::write(h.home.join("code/notes.txt"), "not a folder").unwrap();

    let r = h.api("fs.browse", json!({"path": "~"})).unwrap();
    assert_eq!(r["path"], h.home.to_str().unwrap(), "{r}");
    assert_eq!(r["git_repo"], false);
    let names: Vec<&str> = r["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(names.contains(&"code"), "{r}");

    let r = h.api("fs.browse", json!({"path": "~/code"})).unwrap();
    let entries = r["entries"].as_array().unwrap();
    let entry = |n: &str| entries.iter().find(|e| e["name"] == n).cloned();
    assert_eq!(entry("proj").unwrap()["git_repo"], true, "{r}");
    assert_eq!(entry("plain").unwrap()["git_repo"], false, "{r}");
    assert!(entry("notes.txt").is_none(), "only folders: {r}");
    assert_eq!(r["parent"], h.home.to_str().unwrap());
    // A prefix narrows the listing.
    let r = h
        .api("fs.browse", json!({"path": "~/code", "prefix": "pr"}))
        .unwrap();
    assert_eq!(r["entries"].as_array().unwrap().len(), 1, "{r}");
    // A repository itself says so.
    let r = h
        .api("fs.browse", json!({"path": proj.to_str().unwrap()}))
        .unwrap();
    assert_eq!(r["git_repo"], true);

    // Outside HOME (directly or through ..) is refused.
    for path in ["/etc", "~/../.."] {
        let e = h.api("fs.browse", json!({"path": path})).unwrap_err();
        assert_eq!(Host::kind(&e), "permission_denied", "{path}: {e}");
    }
    let e = h
        .api("fs.browse", json!({"path": "relative/dir"}))
        .unwrap_err();
    assert_eq!(Host::kind(&e), "invalid_params", "{e}");
}

#[test]
fn candidates_find_a_workspace_clone_by_origin() {
    let h = Host::new();
    // Outside the scanned source folders: found through the open workspace only.
    let clone = h.home.join("elsewhere/app");
    std::fs::create_dir_all(&clone).unwrap();
    git(&clone, &["init", "-q", "-b", "main"]);
    git(
        &clone,
        &["remote", "add", "origin", "https://github.com/acme/app.git"],
    );
    let other = h.home.join("elsewhere/other");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q", "-b", "main"]);
    git(
        &other,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/other.git",
        ],
    );

    let none = h
        .api(
            "repo.candidates",
            json!({"origin": "git@github.com:acme/app.git"}),
        )
        .unwrap();
    assert_eq!(none["repos"], json!([]), "{none}");

    h.workspace(&clone);
    h.workspace(&other);
    let r = h
        .api(
            "repo.candidates",
            json!({"origin": "git@github.com:acme/app.git"}),
        )
        .unwrap();
    let repos = r["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 1, "{r}");
    assert_eq!(repos[0]["path"], clone.to_str().unwrap());
    assert_eq!(repos[0]["remote"], "https://github.com/acme/app.git");

    let e = h
        .api("repo.candidates", json!({"origin": "not a remote"}))
        .unwrap_err();
    assert_eq!(Host::kind(&e), "invalid_params", "{e}");
}

#[test]
fn cancel_stops_a_queued_send() {
    let h = Host::new();
    let pane = h.workspace(&h.home);
    // The host's gateway publishes the peers it can deliver to.
    let mut gw = Rpc::connect(&h.s.socket());
    gw.call(
        "client.hello",
        json!({"client": "test-gateway", "version": "0", "api": "vibeke/1", "kind": "gateway"}),
    )
    .unwrap();
    gw.call(
        "handoff.peers.set",
        json!({"peers": [{"id": "pr1", "name": "laptop", "owner": "self",
                          "added_at": 1, "expires_at": null, "expired": false}]}),
    )
    .unwrap();

    let r = h
        .api("handoff.send", json!({"pane": pane, "peer": "laptop"}))
        .unwrap();
    let job = r["job"].clone();
    assert_eq!(job["state"], "queued", "{r}");
    assert_eq!(job["peer"], "pr1");
    let id = job["id"].as_str().unwrap().to_string();

    let r = h.api("handoff.cancel", json!({"id": id})).unwrap();
    assert_eq!(r["job"]["state"], "cancelled", "{r}");
    let jobs = h.api("handoff.jobs", json!({})).unwrap();
    assert_eq!(jobs["jobs"][0]["id"], id.as_str());
    assert_eq!(jobs["jobs"][0]["state"], "cancelled");
    // The gateway's next report is refused, which stops its worker.
    let e = gw
        .call(
            "handoff.job.update",
            json!({"id": id, "state": "exporting", "actor": "gateway:test"}),
        )
        .unwrap_err();
    assert_eq!(e["data"]["kind"], "conflict", "{e}");
    // Cancelling an unknown job is not_found.
    let e = h.api("handoff.cancel", json!({"id": "nope"})).unwrap_err();
    assert_eq!(Host::kind(&e), "not_found", "{e}");
}

/// A TUI on another machine sends `~` unexpanded: `workspace.create` expands it with the
/// server's home folder and refuses a folder that does not exist (rather than starting the
/// shell in `$HOME` under a workspace rooted somewhere else).
#[test]
fn workspace_create_expands_tilde_and_refuses_a_missing_folder() {
    let h = Host::new();
    std::fs::create_dir_all(h.home.join("code/app")).unwrap();
    let ws = h
        .api(
            "workspace.create",
            json!({"cwd": "~/code/app", "command": ["sleep", "600"]}),
        )
        .unwrap();
    assert_eq!(
        ws["workspace"]["root_path"],
        h.home.join("code/app").to_str().unwrap(),
        "{ws}"
    );
    let e = h
        .api("workspace.create", json!({"cwd": "~/missing"}))
        .unwrap_err();
    assert_eq!(Host::kind(&e), "not_found", "{e}");
    let e = h
        .api("workspace.create", json!({"cwd": "/no/such/folder"}))
        .unwrap_err();
    assert_eq!(Host::kind(&e), "not_found", "{e}");
}
