//! Batch 4 orchestration end to end against a real server in an isolated session: best-of-N
//! families (`task.create` with `agents: "claude:2"`, compare, check, pick, discard), split into
//! task, goal fan-out through the background pass, and the `vm` level on the fake provider.
//!
//! Everything lives in a temp dir under `/tmp`; the harness is a fake `claude` script reached
//! through a minimal PATH, the VM provider is the directory-backed fake. Nothing touches the
//! user's real configuration, repos or `~/.vibeke`. The in-process API tests (gating, claims,
//! the merge queue, learned policy, quota, goals, VM API) are `vk-server/src/orch_tests.rs`.

use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Stand-in for an interactive harness. It announces itself through the real hook shim the way
/// Claude Code does at startup: `agent.spawn` (and so `task.create` with agents, which a goal's
/// background pass uses to start each step) waits up to 30 s for that readiness signal. A fake
/// that never reports holds every step start for the full 30 s, longer than the test waits for
/// the next step; it only went unnoticed while the run was (wrongly) ended as exited first.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
SID=sess-orch-$$
printf '%s' "{\"session_id\":\"$SID\",\"source\":\"startup\"}" | "$VIBEKE_BIN" hook claude SessionStart >/dev/null 2>&1
sleep 60
"#;

struct Session {
    dir: tempfile::TempDir,
}

fn write_exec(p: &Path, body: &str) {
    std::fs::write(p, body).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Session {
    fn new(extra: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkor")
            .tempdir_in("/tmp")
            .unwrap();
        let d = dir.path().canonicalize().unwrap();
        std::fs::write(
            d.join("config.toml"),
            format!(
                "[preview]\nproxy_port = 0\n\n[tasks]\nroot = \"{}\"\nfetch_before_create = false\n\n{extra}\n",
                d.join("worktrees").display()
            ),
        )
        .unwrap();
        let bin = d.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_exec(&bin.join("claude"), FAKE_CLAUDE);
        // Panes run a login shell, and Debian/Ubuntu's /etc/profile replaces PATH, so the
        // server's PATH alone does not reach the pane: put the fake harness first from the
        // isolated HOME's profile, the way a real install is on a user's PATH.
        let home = d.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".profile"),
            format!("PATH=\"{}:$PATH\"\nexport PATH\n", bin.display()),
        )
        .unwrap();
        Session { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.root();
        let path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", d.join("bin").display());
        std::fs::create_dir_all(d.join("home")).unwrap();
        c.env("HOME", d.join("home"))
            .env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env("VIBEKE_NO_OPEN", "1")
            .env("PATH", path)
            .env("SHELL", "/bin/sh")
            .env("VIBEKE_TEST_HOOKS", "1");
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

    fn api(&self, method: &str, p: Value) -> Result<Value, Value> {
        let out = self
            .cmd(&["api", "call", method, &p.to_string()])
            .output()
            .unwrap();
        if out.status.success() {
            Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
        } else {
            Err(serde_json::from_slice(&out.stderr)
                .unwrap_or_else(|_| json!(String::from_utf8_lossy(&out.stderr))))
        }
    }

    fn ok(&self, method: &str, p: Value) -> Value {
        self.api(method, p.clone())
            .unwrap_or_else(|e| panic!("{method} {p}: {e}"))
    }

    fn until<T>(&self, what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        self.ok("events.read", json!({"types": [kind], "limit": 500}))["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn repo(&self, name: &str) -> PathBuf {
        let r = self.root().join("repos").join(name);
        std::fs::create_dir_all(&r).unwrap();
        git(&r, &["init", "-q", "-b", "main"]);
        std::fs::write(r.join("a.txt"), "a1\na2\n").unwrap();
        std::fs::write(r.join("b.txt"), "b1\n").unwrap();
        git(&r, &["add", "-A"]);
        git(&r, &["commit", "-q", "-m", "init"]);
        r
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn wt(task: &Value) -> PathBuf {
    PathBuf::from(task["worktree_path"].as_str().unwrap())
}

#[test]
fn best_of_n_family_compare_check_pick_and_discard() {
    let s = Session::new(
        "[orchestrate.best_of_n]\nenabled = true\ncheck_command = \"test -f result.txt\"\ncheck = false\n",
    );
    let r = s.repo("app");
    // `--agents claude:2` is `task.create` with a string list.
    let made = s.ok("task.create", json!({"title": "Make it fast", "repo": r, "agents": "claude:2", "prompt": "do the thing", "root": s.root().join("worktrees")}));
    let fam = made["family"]["id"].as_str().unwrap().to_string();
    let kids = made["family"]["children"].as_array().unwrap().clone();
    assert_eq!(kids.len(), 2);
    assert_eq!(kids[0]["handle"], format!("{fam}.1"));
    assert_eq!(kids[1]["handle"], format!("{fam}.2"));
    let (t1, t2) = (made["tasks"][0].clone(), made["tasks"][1].clone());
    assert_eq!(t1["handle"], format!("{fam}.1"));
    assert_ne!(t1["branch"], t2["branch"], "each child has its own branch");
    assert_ne!(wt(&t1), wt(&t2));
    assert_eq!(made["runs"].as_array().unwrap().len(), 2);
    // The handles resolve like any task handle.
    assert_eq!(
        s.ok("task.get", json!({"task": format!("{fam}.2")}))["task"]["id"],
        t2["id"]
    );
    assert_eq!(
        s.ok("family.list", json!({}))["families"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(s.events("family.created").len(), 1);

    // Child 1 produces the result and commits; child 2 only edits.
    std::fs::write(wt(&t1).join("result.txt"), "ok\n").unwrap();
    std::fs::write(wt(&t1).join("a.txt"), "a1\na2\nfast\n").unwrap();
    git(&wt(&t1), &["add", "-A"]);
    git(&wt(&t1), &["commit", "-q", "-m", "fast"]);
    std::fs::write(wt(&t2).join("a.txt"), "a1\na2\nslow\nslower\nslowest\n").unwrap();

    let before = s.ok("task.compare", json!({"family": fam}));
    assert_eq!(before["reports"].as_array().unwrap().len(), 2);
    assert!(
        before["text"]
            .as_str()
            .unwrap()
            .contains(&format!("{fam}.1"))
    );
    let checked = s.ok("family.check", json!({"family": fam}));
    let results = checked["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    let ok_of =
        |h: &str| results.iter().find(|x| x["child"] == h).unwrap()["outcome"]["ok"].clone();
    assert_eq!(ok_of(&format!("{fam}.1")), true);
    assert_eq!(ok_of(&format!("{fam}.2")), false);
    let cmp = s.ok(
        "task.compare",
        json!({"family": fam, "pair": [format!("{fam}.1"), format!("{fam}.2")]}),
    );
    assert_eq!(cmp["ranking"][0]["handle"], format!("{fam}.1"), "{cmp}");
    assert!(cmp["pair"]["summary"]["files"].is_array());
    // A new commit makes the stored check stale.
    std::fs::write(wt(&t1).join("b.txt"), "b1\nmore\n").unwrap();
    let stale = s.ok("task.compare", json!({"family": fam}));
    let r1 = stale["reports"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["handle"] == format!("{fam}.1"))
        .unwrap()
        .clone();
    assert_eq!(r1["check_stale"], true);

    // Pick the winner and discard the loser.
    let e = s
        .api("task.pick", json!({"family": fam, "child": "nope"}))
        .unwrap_err();
    assert!(e.to_string().contains("not a child"), "{e}");
    let picked = s.ok(
        "task.pick",
        json!({"family": fam, "child": format!("{fam}.1"), "discard": true, "force": true}),
    );
    assert_eq!(picked["picked"], format!("{fam}.1"));
    assert_eq!(picked["discarded"][0]["ok"], true, "{picked}");
    assert_eq!(s.events("family.picked").len(), 1);
    let again = s
        .api(
            "task.pick",
            json!({"family": fam, "child": format!("{fam}.2")}),
        )
        .unwrap_err();
    assert!(again.to_string().contains("already picked"), "{again}");
    // The loser is archived: its record leaves the live model (so `task.get` no longer finds
    // it), the family keeps the child marked discarded, and the archive is in the event log.
    let fam_now = s.ok("family.get", json!({"family": fam}));
    let kid = |h: String| {
        fam_now["children"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["handle"] == h)
            .cloned()
            .unwrap()
    };
    let (winner, loser) = (kid(format!("{fam}.1")), kid(format!("{fam}.2")));
    assert_eq!(winner["discarded"], false, "{fam_now}");
    assert_eq!(winner["task"]["id"], t1["id"]);
    assert_eq!(loser["discarded"], true, "{fam_now}");
    assert!(loser["task"].is_null(), "{fam_now}");
    assert!(
        s.api("task.get", json!({"task": format!("{fam}.2")}))
            .is_err(),
        "an archived task is no longer live"
    );
    assert!(
        s.events("task.status_changed").iter().any(|e| {
            let e = e.to_string();
            e.contains(t2["id"].as_str().unwrap()) && e.contains("\"archived\"")
        }),
        "the loser's archive is in the event log"
    );
}

#[test]
fn split_moves_uncommitted_work_into_a_new_task_and_cleans_the_source() {
    let s = Session::new("[orchestrate.split]\nenabled = true\nquiet_for = \"100ms\"\n");
    let r = s.repo("shared");
    let ws = s.ok("workspace.create", json!({"cwd": r.to_string_lossy()}));
    // `workspace.create` → `{workspace, tab, root_pane}` (07 §4).
    let pane = ws["root_pane"]["id"].as_str().unwrap().to_string();
    // staged edit, unstaged edit, untracked file.
    std::fs::write(r.join("a.txt"), "a1\na2\nSTAGED\n").unwrap();
    git(&r, &["add", "a.txt"]);
    std::fs::write(r.join("b.txt"), "b1\nUNSTAGED\n").unwrap();
    std::fs::create_dir_all(r.join("new")).unwrap();
    std::fs::write(r.join("new/u.txt"), "u\n").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    let dry = s.ok("task.split", json!({"pane": pane, "dry_run": true}));
    assert_eq!(dry["dry_run"], true);
    assert_eq!(dry["changes"].as_array().unwrap().len(), 3);
    assert_eq!(dry["steps"][0]["step"], "quiesce");
    assert_eq!(
        git(&r, &["status", "--porcelain"]).lines().count(),
        3,
        "a dry run changes nothing"
    );
    let done = s.ok(
        "task.split",
        json!({"pane": pane, "title": "Moved work", "root": s.root().join("worktrees")}),
    );
    assert_eq!(done["moved"].as_array().unwrap().len(), 3);
    let dest = wt(&done["task"]);
    assert_eq!(git(&dest, &["status", "--porcelain"]).lines().count(), 3);
    assert_eq!(
        std::fs::read_to_string(dest.join("a.txt")).unwrap(),
        "a1\na2\nSTAGED\n"
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("new/u.txt")).unwrap(),
        "u\n"
    );
    assert!(
        git(&dest, &["status", "--porcelain"]).contains("A")
            || git(&dest, &["status", "--porcelain"]).contains("M  a.txt")
    );
    assert_eq!(
        git(&r, &["status", "--porcelain"]),
        "",
        "the source is clean again"
    );
    assert_eq!(done["recovery_kept"], false);
    assert!(
        git(&r, &["for-each-ref", "refs/vibeke/split"]).is_empty(),
        "recovery ref dropped after success"
    );
    assert_eq!(s.events("task.split").len(), 1);
    // Nothing left to split.
    let e = s.api("task.split", json!({"pane": pane})).unwrap_err();
    assert!(e.to_string().contains("nothing to split"), "{e}");
}

#[test]
fn an_approved_goal_fans_out_and_advances_as_steps_finish() {
    let s = Session::new("[orchestrate.planner]\nenabled = true\n");
    let r = s.repo("goalrepo");
    let g = s.ok(
        "goal.create",
        json!({"title": "Ship search", "repo": r, "text": "1. add the index\n2. document the index"}),
    );
    assert_eq!(g["goal"]["plan"]["steps"].as_array().unwrap().len(), 2);
    // Nothing runs before approval.
    assert!(
        s.ok("task.list", json!({}))["tasks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let a = s.ok("goal.approve", json!({"goal": "G1", "by": "test"}));
    assert_eq!(a["goal"]["state"], "running");
    let first = a["goal"]["runs"]["s1"]["task"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(a["goal"]["runs"]["s1"]["status"], "running");
    assert!(a["goal"]["runs"]["s2"].is_null(), "step 2 waits for step 1");
    assert_eq!(
        s.ok("task.list", json!({}))["tasks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // Finishing the step's task lets the background pass start the next step.
    s.ok("task.finish", json!({"task": first}));
    let g2 = s.until("step 2 to start", 30, || {
        let g = s.ok("goal.get", json!({"goal": "G1"}));
        (g["goal"]["runs"]["s2"]["status"] == "running").then_some(g)
    });
    assert_eq!(g2["goal"]["runs"]["s1"]["status"], "done");
    let second = g2["goal"]["runs"]["s2"]["task"]
        .as_str()
        .unwrap()
        .to_string();
    s.ok("task.finish", json!({"task": second}));
    let done = s.until("the goal to finish", 30, || {
        let g = s.ok("goal.get", json!({"goal": "G1"}));
        (g["goal"]["state"] == "done").then_some(g)
    });
    assert_eq!(done["progress"], json!({"done": 2, "total": 2}));
    assert_eq!(s.events("goal.step_started").len(), 2);
    assert_eq!(s.events("goal.finished").len(), 1);
    let b = s.ok("goal.briefing", json!({"since": "1h"}));
    assert!(b["briefing"]["text"].as_str().unwrap().contains("Goals"));
}

#[test]
fn the_vm_level_runs_task_panes_through_the_fake_provider() {
    let s = Session::new("[isolation.vm]\nenabled = true\nprovider = \"fake\"\ntemplate = false\n");
    let r = s.repo("vmrepo");
    assert_eq!(s.ok("vm.status", json!({}))["provider"], "fake");
    let made = s.ok("task.create", json!({"title": "In a vm", "repo": r, "isolate": "vm", "setup": false, "root": s.root().join("worktrees")}));
    assert_eq!(made["task"]["isolation"]["level"], "vm", "{made}");
    assert_eq!(made["task"]["isolation"]["provider"], "fake");
    let tid = made["task"]["id"].as_str().unwrap().to_string();
    let vms = s.ok("vm.list", json!({}));
    let vm = &vms["vms"][0];
    assert_eq!(vm["task"], tid);
    assert_eq!(vm["state"], "running");
    // The pane's shell runs through the VM exec wrapper.
    let pane = made["panes"][0]["id"].as_str().unwrap().to_string();
    s.ok(
        "pane.send_text",
        json!({"pane": pane, "text": "echo VM_OK_$VIBEKE_ISOLATION\n"}),
    );
    s.until("the shell inside the vm to answer", 15, || {
        let t = s.ok("pane.read", json!({"pane": pane, "lines": 50}));
        t.to_string().contains("VM_OK_vm").then_some(())
    });
    // Finishing the task destroys its VM.
    s.ok("task.finish", json!({"task": tid}));
    s.until("the vm to be destroyed", 15, || {
        s.ok("vm.list", json!({}))["vms"]
            .as_array()
            .unwrap()
            .is_empty()
            .then_some(())
    });
}
