//! Batch 2B end to end (08 §8, §9, §11.1): `vibeke setup`, `vibeke trust`, and the real TUI under
//! `vibeke debug ptyshot` for the first-run screen and the batch approvals view. Everything runs
//! in a temp dir with an isolated session: `HOME`, `VIBEKE_CONFIG` and every harness config dir
//! point into it, `PATH` has no real harness, and the host terminal is never touched.

use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SAFE_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new(prefix: &str) -> Env {
        let dir = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Env { dir }
    }
    fn d(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }
    fn config(&self) -> PathBuf {
        self.d().join("cfg/config.toml")
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let d = self.d();
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", self.config())
            .env(
                "VIBEKE_NOTIFIER",
                format!("log:{}", d.join("notes.jsonl").display()),
            )
            .env("HOME", d.join("home"))
            .env("PATH", SAFE_PATH)
            .env("CLAUDE_CONFIG_DIR", d.join("h/claude"))
            .env("CODEX_HOME", d.join("h/codex"))
            .env("VIBEKE_PI_HOME", d.join("h/pi"))
            .env("VIBEKE_OMP_HOME", d.join("h/omp"))
            .env("VIBEKE_OPENCODE_HOME", d.join("h/opencode"))
            .env("VIBEKE_GEMINI_HOME", d.join("h/gemini"))
            .env("PS1", "$ ");
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
            "VIBEKE_PANE_ID",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_RUNTIME_DIR",
            "PI_CODING_AGENT_DIR",
            "SSH_CONNECTION",
            "SSH_TTY",
            "SSH_CLIENT",
            "TMUX",
            "LC_TERMINAL",
            "TERM_PROGRAM",
        ] {
            c.env_remove(k);
        }
        c.args(args);
        c
    }
    fn json(&self, args: &[&str]) -> Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let out = self.cmd(&a).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn api(&self, method: &str, p: Value) -> Value {
        self.json(&["api", "call", method, &p.to_string()])
    }
    fn run_stdin(&self, args: &[&str], input: &str) -> (i32, String, String) {
        let mut child = self
            .cmd(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn until<T>(what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(150));
    }
}

#[test]
fn setup_non_interactive_installs_only_what_was_named_into_redirected_dirs() {
    let e = Env::new("vksetup");
    // --dry-run writes nothing anywhere.
    let (code, out, err) = e.run_stdin(
        &[
            "setup",
            "--yes",
            "--dry-run",
            "--install",
            "claude",
            "--notifications",
            "osc",
        ],
        "",
    );
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("settings.json"), "the diff is shown: {out}");
    assert!(!e.d().join("h/claude/settings.json").exists());
    assert!(!e.config().exists());
    // --yes with --install claude: claude only, into the redirected dir.
    let (code, out, err) = e.run_stdin(
        &[
            "setup",
            "--yes",
            "--install",
            "claude",
            "--notifications",
            "osc",
            "--theme",
            "terminal",
        ],
        "",
    );
    assert_eq!(code, 0, "{out}{err}");
    let settings = std::fs::read_to_string(e.d().join("h/claude/settings.json")).unwrap();
    assert!(settings.contains("vibeke"), "{settings}");
    assert!(
        !e.d().join("h/codex/hooks.json").exists(),
        "codex not named"
    );
    assert!(
        !e.d().join("home/.claude").exists(),
        "never the real ~/.claude"
    );
    let cfg = std::fs::read_to_string(e.config()).unwrap();
    assert!(cfg.contains("onboarding = false"), "{cfg}");
    assert!(cfg.contains("channel = \"osc\""), "{cfg}");
    assert!(cfg.contains("name = \"terminal\""), "{cfg}");
}

#[test]
fn setup_interactive_asks_and_defaults_to_no_installs() {
    let e = Env::new("vksetupi");
    // Answers: notifications 3 (none), theme default, write yes. No harness on PATH → no
    // install question.
    let (code, out, err) = e.run_stdin(&["setup"], "3\n\ny\n");
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("== 3. agent integrations =="), "{out}");
    assert!(out.contains("not on PATH"), "{out}");
    assert!(!out.contains("Install the"), "{out}");
    assert!(!e.d().join("h/claude/settings.json").exists());
    let cfg = std::fs::read_to_string(e.config()).unwrap();
    assert!(
        cfg.contains("channel = \"none\"") && cfg.contains("onboarding = false"),
        "{cfg}"
    );
}

#[test]
fn trust_reviews_records_and_a_change_needs_a_new_review() {
    let e = Env::new("vktrust");
    std::fs::create_dir_all(e.config().parent().unwrap()).unwrap();
    std::fs::write(e.config(), "onboarding = false\n").unwrap();
    let repo = e.d().join("repo");
    std::fs::create_dir_all(repo.join(".vibeke")).unwrap();
    std::fs::write(
        repo.join(".vibeke/config.toml"),
        "[tasks]\nbranch_template = \"repo/{slug}\"\n[[keys.command]]\nkey = \"prefix+alt+t\"\ntype = \"popup\"\ncommand = \"make test\"\n[ui]\nanimate = false\n",
    )
    .unwrap();
    let r = repo.to_string_lossy().to_string();
    let (code, out, err) = e.run_stdin(&["trust", &r, "--check"], "");
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("trusted: no"), "{out}");
    assert!(out.contains("warning: ui: ignored"), "{out}");
    assert!(out.contains("make test"), "{out}");
    // Answering no records nothing.
    let (_, out, _) = e.run_stdin(&["trust", &r], "n\n");
    assert!(out.contains("not trusted"), "{out}");
    let v = e.api("policy.trust", json!({"path": r, "check": true}));
    assert_eq!(v["trusted"], false);
    assert_eq!(v["commands"][0]["command"], "make test");
    // --yes trusts the reviewed digest.
    let (code, out, err) = e.run_stdin(&["trust", &r, "--yes"], "");
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("trusted "), "{out}");
    let v = e.api("policy.trust", json!({"path": r, "check": true}));
    assert_eq!(v["trusted"], true, "{v}");
    // An edit (e.g. by an agent) invalidates it.
    std::fs::write(
        repo.join(".vibeke/config.toml"),
        "[tasks]\nport_block = 50\n",
    )
    .unwrap();
    let v = e.api("policy.trust", json!({"path": r, "check": true}));
    assert_eq!(v["trusted"], false, "{v}");
}

/// Run the TUI in a PTY with `keys`; returns (screen, stderr).
fn ptyshot(e: &Env, keys: &str, settle: &str) -> (String, String) {
    let bin = env!("CARGO_BIN_EXE_vibeke");
    let out = e
        .cmd(&[
            "debug", "ptyshot", "--cols", "120", "--rows", "36", "--settle", settle, "--keys",
            keys, "--", bin, "attach",
        ])
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn first_run_screen_walks_to_a_written_config() {
    let e = Env::new("vkonb");
    // No config file: the first-run screen opens by itself.
    assert!(!e.config().exists());
    let keys = "{wait:Welcome to Vibeke}{wait:Your terminal}{enter}\
                {wait:Agent integrations}{enter}\
                {wait:Where should agent notifications go}2{enter}\
                {wait:Theme (previewed live)}{enter}\
                {wait:onboarding = false}{enter}{wait:wrote }{sleep:200}";
    let (screen, err) = ptyshot(&e, keys, "800");
    assert!(
        !err.contains("not on screen"),
        "{err}\n--- screen ---\n{screen}"
    );
    assert!(screen.contains("wrote "), "{screen}");
    let cfg = std::fs::read_to_string(e.config()).unwrap();
    assert!(cfg.contains("onboarding = false"), "{cfg}");
    assert!(cfg.contains("channel = \"osc\""), "{cfg}");
    // Nothing was installed without the explicit `i` + `y`.
    assert!(!e.d().join("h/claude/settings.json").exists());
    // With a config, the next attach starts normally.
    let (screen, err) = ptyshot(&e, "{sleep:800}", "300");
    assert!(!screen.contains("Welcome to Vibeke"), "{screen}{err}");
}

#[test]
fn batch_view_allows_two_equivalent_approvals_at_once() {
    let e = Env::new("vkbatch");
    let d = e.d();
    std::fs::create_dir_all(d.join("cfg/harnesses")).unwrap();
    std::fs::write(
        e.config(),
        "onboarding = false\n[terminal]\ndefault_shell = \"/bin/sh\"\n",
    )
    .unwrap();
    // User manifest: OpenCode approvals are gated and answered natively (as in harnesses.rs).
    std::fs::write(
        d.join("cfg/harnesses/opencode.toml"),
        "id = \"opencode\"\n[[capabilities]]\nversions = \"*\"\nmode = \"tui\"\nobserve = true\ngate = true\nanswer_native = [\"approval\"]\nanswer_keystroke = true\n",
    )
    .unwrap();
    let fake = |n: &str| {
        let result = d.join(format!("decision-{n}.json"));
        let body = format!(
            r#"#!/bin/sh
h() {{ printf '%s' "$2" | "$VIBEKE_BIN" hook opencode "$1"; }}
h session.created '{{"info":{{"id":"ses_{n}","directory":"/tmp"}}}}' >/dev/null
h tool.execute.before '{{"tool":"bash","sessionID":"ses_{n}","callID":"c1","args":{{"command":"pnpm test"}}}}' >/dev/null
OUT=$(h permission.ask '{{"id":"per_{n}","type":"bash","pattern":"pnpm test","title":"Run pnpm test","sessionID":"ses_{n}","callID":"c1","metadata":{{}}}}')
printf '%s' "$OUT" > "{}"
sleep 60
"#,
            result.display()
        );
        let p = d.join(format!("fake-{n}"));
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        (p, result)
    };
    let (f1, r1) = fake("a");
    let (f2, r2) = fake("b");
    // Both agents in one workspace (same policy scope), a plain workspace to focus.
    let agents = d.join("agents");
    let plain = d.join("plainws");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::create_dir_all(&plain).unwrap();
    let p1 =
        e.json(&["workspace", "create", "--cwd", &agents.to_string_lossy()])["root_pane"]["id"]
            .as_str()
            .unwrap()
            .to_string();
    let p2 = e.json(&["pane", "split", &p1, "--direction", "right"])["pane"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    e.json(&["workspace", "create", "--cwd", &plain.to_string_lossy()]);
    for (p, f) in [(&p1, &f1), (&p2, &f2)] {
        let _ = e
            .cmd(&[
                "pane",
                "wait-idle",
                p,
                "--quiet-ms",
                "300",
                "--timeout-ms",
                "10000",
            ])
            .output();
        e.json(&["pane", "run", p, &f.to_string_lossy()]);
    }
    until("two gated approvals", 20, || {
        let v = e.api("interaction.list", json!({}));
        let n = v["interactions"]
            .as_array()?
            .iter()
            .filter(|i| i["status"] == "Open" && i["gate"] == true)
            .count();
        (n == 2).then_some(())
    });
    // Focus the plain workspace, open the batch view from the palette, allow all.
    let keys = "{sleep:800}{ctrl+b}gplainws{sleep:300}{enter}{sleep:300}\
                {ctrl+b}:batch approvals{sleep:300}{enter}\
                {wait:allow all 2}a{wait:delivered}{sleep:300}";
    let (screen, err) = ptyshot(&e, keys, "500");
    assert!(
        !err.contains("not on screen"),
        "{err}\n--- screen ---\n{screen}"
    );
    for r in [&r1, &r2] {
        let out = until("decision file", 15, || {
            std::fs::read_to_string(r).ok().filter(|t| !t.is_empty())
        });
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap(),
            json!({"status": "allow"})
        );
    }
    let _ = Path::new("");
}

fn git(dir: &Path, args: &[&str]) {
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
}

#[test]
fn trusted_repo_tasks_settings_apply_to_new_tasks_only_after_trust() {
    let e = Env::new("vktrusttask");
    std::fs::create_dir_all(e.config().parent().unwrap()).unwrap();
    std::fs::write(e.config(), "onboarding = false\n[tasks]\ncopy_files = []\n").unwrap();
    let repo = e.d().join("repo");
    std::fs::create_dir_all(repo.join(".vibeke")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "x\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "local.env\n").unwrap();
    std::fs::write(
        repo.join(".vibeke/config.toml"),
        "[tasks]\ncopy_files = [\"local.env\"]\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    std::fs::write(repo.join("local.env"), "SECRET=1\n").unwrap();
    let create = |title: &str| {
        let r = e.api(
            "task.create",
            json!({"title": title, "repo": repo.to_string_lossy(), "root": e.d().join("wt").to_string_lossy(), "fetch": false}),
        );
        PathBuf::from(r["task"]["worktree_path"].as_str().unwrap())
    };
    let wt = create("before trust");
    assert!(
        !wt.join("local.env").exists(),
        "untrusted repo config is ignored"
    );
    let r = repo.to_string_lossy().to_string();
    let (code, out, err) = e.run_stdin(&["trust", &r, "--yes"], "");
    assert_eq!(code, 0, "{out}{err}");
    let wt = create("after trust");
    assert!(
        wt.join("local.env").exists(),
        "trusted [tasks] copy_files applied"
    );
}
