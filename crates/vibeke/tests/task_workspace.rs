//! Task workspace features end to end against a real server in an isolated session: the repo's
//! `.vibeke/task.toml` (files, deps, ports, env, setup), repo trust for repo-provided commands,
//! the visible setup pane and its agent-start flags, user overrides, the reconcile loop,
//! `vibeke doctor`'s port-pool warning and PR status through a fake `gh`.
//!
//! Everything lives in a temp dir under `/tmp`: the session's runtime and state dirs, the config,
//! the repos and the task worktrees (asserted). The user's real repos and `~/.vibeke` are never
//! touched. The fake `gh` and the fake `claude` are scripts in the temp dir, reached through an
//! absolute override honoured only under `VIBEKE_TEST_HOOKS=1` and through `PATH`.

use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct Session {
    dir: tempfile::TempDir,
}

fn write_exec(p: &Path, body: &str) {
    std::fs::write(p, body).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Session {
    /// `tasks_extra`: extra lines for the `[tasks]` table; `more`: further tables.
    fn new(tasks_extra: &str, more: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vktw")
            .tempdir_in("/tmp")
            .unwrap();
        let d = dir.path().canonicalize().unwrap();
        std::fs::write(
            d.join("config.toml"),
            format!(
                "[preview]\nproxy_port = 0\n\n[tasks]\nroot = \"{}\"\nfetch_before_create = false\n{tasks_extra}\n{more}\n",
                d.join("worktrees").display()
            ),
        )
        .unwrap();
        // A fake `claude`: records when it started and whether setup had finished by then.
        let bin = d.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_exec(
            &bin.join("claude"),
            r#"#!/bin/sh
d="$(pwd)"
log="$(dirname "$0")/../claude.log"
if [ -f "$d/setup-done" ]; then echo "agent started after setup" >> "$log"; else echo "agent started before setup finished" >> "$log"; fi
sleep 60
"#,
        );
        Session { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.root();
        // Minimal PATH: only the fake `claude` is reachable, no real harness binary.
        let path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", d.join("bin").display());
        // HOME in the temp dir too: `sh -l` must not read the real ~/.profile (which would put
        // the real `claude` first), and nothing may touch the real ~/.vibeke.
        std::fs::create_dir_all(d.join("home")).unwrap();
        c.env("HOME", d.join("home"))
            .env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env("VIBEKE_NO_OPEN", "1")
            .env("PATH", path)
            // A plain `sh` in the panes: no rc files that put the real `claude` first.
            .env("SHELL", "/bin/sh")
            .env("VIBEKE_TEST_HOOKS", "1")
            .env("VIBEKE_GH_BIN", d.join("gh"));
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
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    /// All events of `kind`.
    fn events(&self, kind: &str) -> Vec<Value> {
        self.ok("events.read", json!({"types": [kind], "limit": 500}))["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn task(&self, id: &str) -> Value {
        self.ok("task.get", json!({"task": id}))
    }

    fn create(&self, repo: &Path, title: &str, extra: Value) -> Value {
        let mut p = json!({
            "title": title,
            "repo": repo.to_string_lossy(),
            "root": self.root().join("worktrees").to_string_lossy(),
        });
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            p[k] = v;
        }
        let r = self.ok("task.create", p);
        let wt = r["task"]["worktree_path"].as_str().unwrap();
        assert!(
            Path::new(wt).starts_with(self.root()),
            "the task worktree must live in the test's temp dir: {wt}"
        );
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

/// A committed repo with `.vibeke/task.toml` = `task_toml` and some ignored files.
fn repo(s: &Session, name: &str, task_toml: &str) -> PathBuf {
    let r = s.root().join("repos").join(name);
    std::fs::create_dir_all(r.join(".vibeke")).unwrap();
    git(&r, &["init", "-q", "-b", "main"]);
    std::fs::write(r.join("a.txt"), "a\n").unwrap();
    std::fs::write(r.join(".gitignore"), ".env\nnode_modules\ndata\n").unwrap();
    std::fs::write(r.join(".vibeke/task.toml"), task_toml).unwrap();
    git(&r, &["add", "-A"]);
    git(&r, &["commit", "-q", "-m", "init"]);
    // Untracked/ignored material the task needs.
    std::fs::write(r.join(".env"), "SECRET=hunter2\n").unwrap();
    std::fs::create_dir_all(r.join("data")).unwrap();
    std::fs::write(r.join("data/fixture"), "big").unwrap();
    std::fs::create_dir_all(r.join("node_modules/dep")).unwrap();
    std::fs::write(r.join("node_modules/dep/i.js"), "1").unwrap();
    r
}

fn wt_of(task: &Value) -> PathBuf {
    PathBuf::from(task["task"]["worktree_path"].as_str().unwrap())
}

#[test]
fn task_file_materializes_files_and_setup_waits_for_trust() {
    let s = Session::new("", "");
    let r = repo(
        &s,
        "app",
        r#"
[files]
copy = [".env", ".env.missing"]
link = ["data"]
clone = ["node_modules"]

[deps]
strategy = "clone"

[setup]
run = ['echo "db=$DB api=$API_PORT slug={slug} port={port}" > setup-ran.txt']

[ports]
count = 5
env = { API_PORT = 1 }

[env]
DB = "app_{slug_underscored}"
"#,
    );
    let created = s.create(&r, "Fix Login Redirect", json!({}));
    let wt = wt_of(&created);
    let tid = created["task"]["id"].as_str().unwrap().to_string();
    let slug = created["task"]["slug"].as_str().unwrap().to_string();

    // Files: copied (a real file, never a symlink), linked, cloned; only names and hashes reported.
    assert_eq!(
        std::fs::read_to_string(wt.join(".env")).unwrap(),
        "SECRET=hunter2\n"
    );
    assert!(
        !std::fs::symlink_metadata(wt.join(".env"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(std::fs::read_link(wt.join("data")).unwrap(), r.join("data"));
    assert_eq!(
        std::fs::read_to_string(wt.join("node_modules/dep/i.js")).unwrap(),
        "1"
    );
    let ev = s.events("task.files_materialized");
    assert_eq!(ev.len(), 1);
    let files = ev[0]["data"]["files"].as_array().unwrap();
    let outcome = |p: &str| files.iter().find(|f| f["path"] == p).unwrap()["outcome"].clone();
    assert_eq!(outcome(".env"), "copied");
    assert_eq!(outcome("data"), "linked");
    assert_eq!(outcome("node_modules"), "cloned");
    assert_eq!(outcome(".env.missing"), "missing");
    assert!(
        files.iter().find(|f| f["path"] == ".env").unwrap()["hash"].is_string(),
        "{files:?}"
    );
    assert!(
        !ev[0].to_string().contains("hunter2"),
        "event must not carry file contents"
    );

    // `[ports] count` sizes the lease.
    let range = created["task"]["port_range"].as_array().unwrap();
    assert_eq!(range[1].as_u64().unwrap() - range[0].as_u64().unwrap(), 4);

    // Untrusted: nothing ran, the commands are shown in the event, no setup pane exists.
    assert_eq!(created["task"]["setup_status"], "untrusted");
    assert!(!wt.join("setup-ran.txt").exists());
    let un = s.events("task.setup_untrusted");
    assert_eq!(un.len(), 1);
    let shown = un[0]["data"]["commands"].to_string();
    assert!(
        shown.contains("setup.run") && shown.contains("setup-ran.txt"),
        "{shown}"
    );
    assert!(
        un[0]["data"]["hint"]
            .as_str()
            .unwrap()
            .contains("policy trust")
    );

    // `policy trust` shows what it is trusting: every command of the task file.
    let trust = s.ok("policy.trust", json!({"path": r.to_string_lossy()}));
    assert!(
        trust["task_file"]["commands"]
            .to_string()
            .contains("setup-ran.txt"),
        "{trust}"
    );

    // Rerun: now it runs, in a visible `setup` pane, with the templated env and port mapping.
    let rerun = s.ok("task.setup", json!({"task": tid}));
    assert_eq!(rerun["started"], true, "{rerun}");
    let setup_pane = rerun["pane"].as_str().unwrap().to_string();
    let title = s.ok("pane.get", json!({"pane": setup_pane}))["pane"]["title"].clone();
    assert_eq!(title, "setup");
    s.until("setup finished", 20, || {
        (s.task(&tid)["task"]["setup_status"] == "Succeeded").then_some(())
    });
    let base = range[0].as_u64().unwrap();
    assert_eq!(
        std::fs::read_to_string(wt.join("setup-ran.txt"))
            .unwrap()
            .trim(),
        format!(
            "db=app_{} api={} slug={slug} port={base}",
            slug.replace('-', "_"),
            base + 1
        )
    );
    assert_eq!(s.events("task.setup_started").len(), 1);
    assert_eq!(s.events("task.setup_finished").len(), 1);
    assert!(s.events("task.setup_failed").is_empty());

    // A changed `.vibeke/` tree is untrusted again.
    std::fs::write(
        wt.join(".vibeke/task.toml"),
        "[setup]\nrun = ['echo changed']\n",
    )
    .unwrap();
    let again = s.ok("task.setup", json!({"task": tid}));
    assert_eq!(again["started"], false);
    assert_eq!(again["needs_trust"], true);
}

#[test]
fn user_overrides_run_without_trust_and_the_repos_env_is_withheld() {
    let s = Session::new("", "");
    let r = repo(
        &s,
        "ovr",
        r#"
[setup]
run = ['echo "repo-ran" > repo-ran.txt']
[env]
REPO_ENV = "from-repo"
"#,
    );
    // The user's own config for this repo (keyed by its path): their commands are trusted.
    std::fs::write(
        s.root().join("config.toml"),
        format!(
            "[preview]\nproxy_port = 0\n[tasks]\nroot = \"{root}\"\nfetch_before_create = false\n\n[tasks.repos.\"{repo}\"]\nenv = {{ USER_ENV = \"mine-{{slug}}\" }}\nsetup = {{ run = ['echo \"user=$USER_ENV repo=$REPO_ENV\" > user-ran.txt'] }}\n",
            root = s.root().join("worktrees").display(),
            repo = r.display()
        ),
    )
    .unwrap();
    let created = s.create(&r, "override me", json!({}));
    let wt = wt_of(&created);
    let tid = created["task"]["id"].as_str().unwrap().to_string();
    let slug = created["task"]["slug"].as_str().unwrap().to_string();
    s.until("user setup ran", 20, || {
        wt.join("user-ran.txt").exists().then_some(())
    });
    s.until("setup finished", 20, || {
        (s.task(&tid)["task"]["setup_status"] == "Succeeded").then_some(())
    });
    // The user's env applied; the untrusted repo's `[env]` and its own commands did not.
    assert_eq!(
        std::fs::read_to_string(wt.join("user-ran.txt"))
            .unwrap()
            .trim(),
        format!("user=mine-{slug} repo=")
    );
    assert!(!wt.join("repo-ran.txt").exists());
}

fn trusted_repo(s: &Session, name: &str, setup: &str) -> PathBuf {
    let r = repo(s, name, setup);
    s.ok("policy.trust", json!({"path": r.to_string_lossy()}));
    r
}

fn agent_log(s: &Session) -> String {
    std::fs::read_to_string(s.root().join("claude.log")).unwrap_or_default()
}

#[test]
fn agents_wait_for_setup_and_follow_the_setup_flags() {
    let s = Session::new("", "");
    let agent = json!({"agents": [{"harness": "claude"}]});

    // Default: the agent starts only after setup succeeded.
    let ok = trusted_repo(&s, "ok", "[setup]\nrun = ['sleep 1', 'touch setup-done']\n");
    let r = s.create(&ok, "waits", agent.clone());
    assert_eq!(r["setup"]["agents_pending"], true, "{r}");
    assert!(r["runs"].as_array().unwrap().is_empty());
    s.until("agent started", 30, || {
        agent_log(&s).contains("agent started").then_some(())
    });
    assert!(
        agent_log(&s).contains("agent started after setup"),
        "{}",
        agent_log(&s)
    );

    // Failing setup: no agent, a `setup` pane stays, a notification fires.
    let bad = trusted_repo(&s, "bad", "[setup]\nrun = ['exit 7']\n");
    let before = agent_log(&s).matches("agent started").count();
    let r = s.create(&bad, "fails", agent.clone());
    let tid = r["task"]["id"].as_str().unwrap().to_string();
    s.until("setup failed", 20, || {
        (!s.events("task.setup_failed").is_empty()).then_some(())
    });
    let failed = s.events("task.setup_failed");
    assert_eq!(failed[0]["data"]["exit_code"], 7);
    s.until("agents withheld", 10, || {
        (!s.events("task.agents_withheld").is_empty()).then_some(())
    });
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        agent_log(&s).matches("agent started").count(),
        before,
        "no agent after a failed setup"
    );
    let panes = s.ok("pane.list", json!({}))["panes"].clone();
    assert!(
        panes
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["title"] == "setup" && p["workspace"] == r["workspace"]["id"]),
        "the setup pane stays open: {panes}"
    );
    let notes = s.ok("notification.list", json!({"unread_only": false}));
    assert!(notes.to_string().contains("setup failed"), "{notes}");
    assert_eq!(
        s.task(&tid)["task"]["setup_status"],
        "Failed { exit_code: Some(7) }"
    );

    // `start_agents_on_failure = true`: the agent starts anyway.
    let force = trusted_repo(
        &s,
        "force",
        "[setup]\nrun = ['exit 1']\nstart_agents_on_failure = true\n",
    );
    s.create(&force, "forced", agent.clone());
    s.until("agent started despite failure", 30, || {
        (agent_log(&s).matches("agent started").count() > before).then_some(())
    });

    // `parallel_agent = true`: the agent starts immediately, while setup is still running.
    let par = trusted_repo(
        &s,
        "par",
        "[setup]\nrun = ['sleep 4', 'touch setup-done']\nparallel_agent = true\n",
    );
    let n = agent_log(&s).matches("agent started").count();
    let r = s.create(&par, "parallel", agent);
    assert_eq!(r["setup"]["agents_pending"], false, "{r}");
    assert_eq!(r["runs"].as_array().unwrap().len(), 1);
    s.until("parallel agent started", 15, || {
        (agent_log(&s).matches("agent started").count() > n).then_some(())
    });
    assert!(
        agent_log(&s).contains("agent started before setup finished"),
        "{}",
        agent_log(&s)
    );
}

#[test]
fn reconcile_marks_missing_and_reports_orphans_but_deletes_nothing() {
    let s = Session::new("", "");
    let r = repo(&s, "rec", "");
    let created = s.create(&r, "will vanish", json!({"setup": false}));
    let tid = created["task"]["id"].as_str().unwrap().to_string();
    let wt = wt_of(&created);
    let branch = created["task"]["branch"].as_str().unwrap().to_string();

    // A worktree made outside Vibeke inside the task root, and a stray directory.
    let manual = s.root().join("worktrees/rec/manual");
    git(
        &r,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "manual-branch",
            manual.to_str().unwrap(),
        ],
    );
    let stray = s.root().join("worktrees/rec/leftover");
    std::fs::create_dir_all(&stray).unwrap();
    std::fs::write(stray.join("keep.txt"), "x").unwrap();

    // The task's checkout disappears behind Vibeke's back.
    std::fs::remove_dir_all(&wt).unwrap();

    let rep = s.ok("task.reconcile", json!({"repo": r.to_string_lossy()}));
    let report = &rep["reports"][0];
    assert_eq!(report["missing"][0]["task_id"], tid.as_str(), "{rep}");
    let orphans = report["orphans"].to_string();
    assert!(
        orphans.contains("manual") && orphans.contains("leftover"),
        "{orphans}"
    );
    assert_eq!(s.task(&tid)["task"]["status"], "missing");
    assert_eq!(s.events("task.missing").len(), 1);
    assert_eq!(s.events("worktree.orphan_found").len(), 2);

    // Nothing was deleted: the orphans and the task's branch are intact.
    assert!(manual.is_dir() && stray.join("keep.txt").is_file());
    assert!(git(&r, &["branch", "--list", &branch]).contains(&branch));
    assert!(git(&r, &["branch", "--list", "manual-branch"]).contains("manual-branch"));

    // A second pass announces nothing new for the same problems.
    s.ok("task.reconcile", json!({"repo": r.to_string_lossy()}));
    assert_eq!(s.events("task.missing").len(), 1);
}

#[test]
fn pr_status_through_a_fake_gh_is_cached_and_never_prompts() {
    let s = Session::new("", "");
    let r = repo(&s, "prs", "");
    let created = s.create(&r, "has a pr", json!({"setup": false}));
    let tid = created["task"]["id"].as_str().unwrap().to_string();
    let calls = s.root().join("gh-calls.log");
    write_exec(
        &s.root().join("gh"),
        &format!(
            r#"#!/bin/sh
echo "$@" >> "{calls}"
[ "$GH_PROMPT_DISABLED" = "1" ] || exit 9
case "$1 $2" in
  "auth status") [ -f "{unauth}" ] && exit 1; exit 0 ;;
  "pr view") echo '{{"number":7,"state":"OPEN","isDraft":false,"reviewDecision":"","url":"https://example.invalid/pull/7","statusCheckRollup":[{{"status":"COMPLETED","conclusion":"FAILURE"}}]}}' ;;
esac
"#,
            calls = calls.display(),
            unauth = s.root().join("unauth").display()
        ),
    );
    // Nothing is fetched by a plain task.get (cache only).
    assert!(s.task(&tid)["pr"].is_null());
    assert!(!calls.exists());

    let pr = s.ok("task.pr", json!({"task": tid}));
    assert_eq!(pr["pr"]["kind"], "pr");
    assert_eq!(pr["pr"]["pr"]["label"], "#7 ✗ checks");
    assert_eq!(pr["pr"]["pr"]["url"], "https://example.invalid/pull/7");
    let n_calls = || {
        std::fs::read_to_string(&calls)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("pr view"))
            .count()
    };
    assert_eq!(n_calls(), 1);
    // Cached for 60 s: a repeat and `task.get` do not call gh again.
    s.ok("task.pr", json!({"task": tid}));
    assert_eq!(s.task(&tid)["pr"]["pr"]["number"], 7);
    assert_eq!(n_calls(), 1);
    s.ok("task.pr", json!({"task": tid, "refresh": true}));
    assert_eq!(n_calls(), 2);

    // Not authenticated: unavailable, and `pr view` is not even attempted.
    std::fs::write(s.root().join("unauth"), "").unwrap();
    let r2 = s.create(&r, "second", json!({"setup": false}));
    let t2 = r2["task"]["id"].as_str().unwrap().to_string();
    let un = s.ok("task.pr", json!({"task": t2}));
    assert_eq!(un["pr"]["kind"], "unavailable");
    assert!(
        un["pr"]["reason"]
            .as_str()
            .unwrap()
            .contains("not authenticated")
    );
    assert_eq!(n_calls(), 2);
}

#[test]
fn doctor_warns_when_the_port_pool_is_exhausted_and_a_task_starts_without_ports() {
    // A pool of exactly one block, on ports nothing else is using right now (a fixed range
    // collides with other tests and listeners on a busy host).
    let base = free_block(10);
    let s = Session::new(
        &format!("port_pool = \"{base}-{}\"\nport_block = 10\n", base + 9),
        "",
    );
    let r = repo(&s, "ports", "");
    let first = s.create(&r, "first", json!({"setup": false}));
    assert_eq!(first["task"]["port_range"][0], base);
    // The second finds no block: it still gets created, with a warning and no range.
    let second = s.create(&r, "second", json!({"setup": false}));
    assert!(second["task"]["port_range"].is_null(), "{second}");
    assert!(
        second["warnings"].to_string().contains("no ports leased"),
        "{second}"
    );

    let out = s.cmd(&["doctor", "--no-remote"]).output().unwrap();
    let report: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    let tasks: Vec<&Value> = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["section"] == "tasks")
        .collect();
    assert!(
        tasks
            .iter()
            .any(|c| c["level"] == "warn" && c["message"].as_str().unwrap().contains("exhausted")),
        "{tasks:?}"
    );
}

/// The first of `n` consecutive loopback ports that are all free, searched from a
/// pid-dependent start so parallel test processes don't race for the same block.
fn free_block(n: u16) -> u16 {
    let start = 30_000 + (std::process::id() % 1_000) as u16 * 20;
    (0..2_000u16)
        .map(|i| 30_000 + (start - 30_000 + i * n) % 20_000)
        .find(|&b| (b..b + n).all(|p| std::net::TcpListener::bind(("127.0.0.1", p)).is_ok()))
        .expect("no free port block")
}
