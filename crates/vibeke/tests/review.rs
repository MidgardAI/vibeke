//! Spec 15 T2/T3 end to end with a fake Claude that reports through the real hook shim and
//! commits in a throwaway repository: track → work → review package (claim vs observed command)
//! → explicit per-candidate authorization → verification in a disposable checkout → Ready →
//! accept → a later commit outdates the acceptance; the attention inbox ranks the review.

use serde_json::{Value, json};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const FAKE_CLAUDE: &str = r#"#!/bin/sh
# Stand-in for an interactive harness: an input box, prompts/tools/stops reported via hooks.
SID=sess-r
h() { printf '%s' "$2" | "$VIBEKE_BIN" hook claude "$1" >/dev/null 2>&1; }
q() { python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'; }
G="env GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 git -c user.name=fake -c user.email=fake@example.invalid -c commit.gpgsign=false"
h SessionStart "{\"session_id\":\"$SID\",\"source\":\"startup\"}"
n=0
while true; do
  printf '\n╭──────────────────╮\n│ > '
  IFS= read -r line || exit 0
  n=$((n+1))
  P=$(printf '%s' "$line" | q)
  h UserPromptSubmit "{\"session_id\":\"$SID\",\"prompt\":$P}"
  case "$line" in
    fix*) echo pass > status.txt; $G add -A >/dev/null 2>&1; $G commit -q -m "fix $n" >/dev/null 2>&1 ;;
    more*) echo "$n" >> notes.txt; $G add -A >/dev/null 2>&1; $G commit -q -m "more $n" >/dev/null 2>&1 ;;
  esac
  T="t$n-$$"
  h PreToolUse "{\"session_id\":\"$SID\",\"tool_name\":\"Bash\",\"tool_use_id\":\"$T\",\"tool_input\":{\"command\":\"sh ci/test.sh\"}}"
  sh ci/test.sh >/dev/null 2>&1; RC=$?
  h PostToolUse "{\"session_id\":\"$SID\",\"tool_name\":\"Bash\",\"tool_use_id\":\"$T\",\"tool_response\":{\"exit_code\":$RC}}"
  echo "worked on: $line"
  h Stop "{\"session_id\":\"$SID\",\"last_assistant_message\":\"Done. All tests pass.\"}"
done
"#;

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new() -> Self {
        Session {
            dir: tempfile::Builder::new()
                .prefix("vkreview")
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
    fn json(&self, args: &[&str]) -> Value {
        let out = self.cmd(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
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
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn review_loop_with_verification_and_acceptance() {
    let s = Session::new();
    let repo = s.dir.path().join("repo");
    std::fs::create_dir_all(repo.join("ci")).unwrap();
    std::fs::create_dir_all(repo.join(".vibeke")).unwrap();
    std::fs::write(
        repo.join(".vibeke/config.toml"),
        "[[task.checks]]\nname = \"unit\"\ncommand = \"sh ci/test.sh\"\ntimeout_s = 60\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("ci/test.sh"),
        "#!/bin/sh\ngrep -q pass status.txt\n",
    )
    .unwrap();
    std::fs::write(repo.join("status.txt"), "fail\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let script = s.dir.path().join("fake-claude");
    std::fs::write(&script, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let pane =
        s.json(&["workspace", "create", "--cwd", &repo.to_string_lossy()])["root_pane"]["id"]
            .as_str()
            .unwrap()
            .to_string();
    let _ = s
        .cmd(&[
            "pane",
            "wait-idle",
            &pane,
            "--quiet-ms",
            "600",
            "--timeout-ms",
            "10000",
        ])
        .output();
    s.json(&["pane", "run", &pane, &script.to_string_lossy()]);
    let run = s.until("hook-bound run", 15, || {
        let v = s.api("task.sources", json!({"pane": pane})).ok()?;
        (v["identity_verified"] == true).then(|| v["run"].as_str().unwrap().to_string())
    });
    s.json(&["pane", "run", &pane, "Make the unit tests pass"]);
    s.until("recorded turn", 10, || {
        let v = s.api("task.sources", json!({"run": run})).ok()?;
        (!v["turns"].as_array()?.is_empty()).then_some(())
    });
    let tracked = s
        .api(
            "task.track",
            json!({"run": run, "criteria": [{"text": "Unit tests pass", "checks": ["unit"]}], "stop_at": "implementation"}),
        )
        .unwrap();
    let task = tracked["task"]["id"].as_str().unwrap().to_string();

    // The agent fixes and commits; its "All tests pass" is only a claim.
    s.json(&["pane", "run", &pane, "fix it"]);
    let pkg = s.until("committed candidate with observations", 15, || {
        let p = s.api("task.review.get", json!({"task": task})).ok()?;
        (p["subject"].is_object() && !p["observed_commands"].as_array()?.is_empty()).then_some(p)
    });
    assert_eq!(pkg["claims"][0]["category"], "agent_claim");
    assert_eq!(pkg["claims"][0]["command"], "Done. All tests pass.");
    let obs = pkg["observed_commands"].as_array().unwrap().last().unwrap();
    assert_eq!(obs["command"], "sh ci/test.sh");
    assert_eq!(obs["label"], "Command passed · code binding unverified");
    assert_ne!(pkg["label"], "ready_for_review");
    let subject = pkg["subject"]["id"].as_str().unwrap().to_string();

    // Verification needs the user's per-candidate authorization; then it runs off-host-tree.
    let e = s
        .api(
            "task.check.run",
            json!({"task": task, "check": "unit", "subject": subject}),
        )
        .unwrap_err();
    assert_eq!(
        e["error"]["details"]["reason"], "authorization_required",
        "{e}"
    );
    let auth = s
        .api(
            "task.check.authorize",
            json!({"task": task, "check": "unit", "subject": subject, "idempotency_key": "auth-1"}),
        )
        .unwrap();
    assert_eq!(
        auth["confirmation_label"],
        "Runs code modified by this task"
    );
    let started = s
        .api(
            "task.check.run",
            json!({"task": task, "check": "unit", "subject": subject, "idempotency_key": "run-1"}),
        )
        .unwrap();
    let cr = started["check_run"]["id"].as_str().unwrap().to_string();
    // Same key → same run, no second launch.
    let again = s
        .api(
            "task.check.run",
            json!({"task": task, "check": "unit", "subject": subject, "idempotency_key": "run-1"}),
        )
        .unwrap();
    assert_eq!(again["replayed"], true);
    assert_eq!(again["check_run"]["id"], cr.as_str());
    let done = s.until("check finished", 60, || {
        let g = s.api("task.check.get", json!({"check_run": cr})).ok()?;
        let st = g["check_run"]["state"].as_str()?.to_string();
        (st != "queued" && st != "running").then_some(g)
    });
    assert_eq!(done["check_run"]["state"], "passed", "{done}");

    let pkg = s.until("ready", 15, || {
        let p = s.api("task.review.get", json!({"task": task})).ok()?;
        (p["label"] == "ready_for_review").then_some(p)
    });
    assert_eq!(pkg["actions"]["accept"]["available"], true);

    // The inbox shows the review candidate (class 4) for this task.
    let inbox = s.until("review in inbox", 10, || {
        let v = s.api("attention.list", json!({"budget_ms": 300000})).ok()?;
        v["items"]
            .as_array()?
            .iter()
            .any(|i| i["key"]["kind"] == "review" && i["task"] == task.as_str())
            .then_some(v)
    });
    let item = inbox["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"]["kind"] == "review")
        .unwrap();
    assert_eq!(item["class"], 4);
    assert_eq!(item["title"], tracked["task"]["title"]);
    assert!(inbox["five_minute"]["keys"].is_array());

    // Accept the exact revision shown.
    let acc = s
        .api(
            "task.review.accept",
            json!({"task": task, "intent_revision": 1, "subject_id": subject, "idempotency_key": "acc-1"}),
        )
        .unwrap();
    assert_eq!(acc["label"], "reviewed");
    let receipt = s
        .api("task.operation.get", json!({"idempotency_key": "acc-1"}))
        .unwrap();
    assert_eq!(receipt["known"], true);

    // A later commit outdates the acceptance, visibly.
    s.json(&["pane", "run", &pane, "more polish"]);
    let pkg = s.until("acceptance outdated", 15, || {
        let p = s.api("task.review.get", json!({"task": task})).ok()?;
        (p["label"] == "review_outdated").then_some(p)
    });
    assert_eq!(pkg["acceptance"]["status"], "outdated");
    let t = s.api("task.detail", json!({"task": task})).unwrap();
    assert_eq!(t["task"]["review_label"], "review_outdated");
}
