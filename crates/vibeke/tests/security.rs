//! Server security end to end (09), through the real binary: runtime-dir checks and
//! `umask 077`, `auth.elevate` from inside a pane approved outside it, `pane revoke-token`,
//! `policy add|list|test|remove|trust --allow-policy-grants`, the audit log and
//! `doctor --audit`, integration tamper detection (`integration.doctor`) against temp harness
//! dirs, and `debug bundle`. In-pane commands write their output to files in the session dir,
//! so nothing depends on scraping the screen.

mod support;

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use support::Session;

/// `vibeke` with the session's dirs, harness configs redirected into the session dir (never the
/// user's real ones) and a known umask (022) for the server it may spawn.
fn cmd(s: &Session, args: &[&str]) -> Command {
    let mut c = s.cmd(args);
    let d = s.dir.path();
    for (k, sub) in [
        ("CLAUDE_CONFIG_DIR", "h/claude"),
        ("CODEX_HOME", "h/codex"),
        ("VIBEKE_PI_HOME", "h/pi"),
        ("VIBEKE_OMP_HOME", "h/omp"),
        ("VIBEKE_OPENCODE_HOME", "h/opencode"),
        ("VIBEKE_GEMINI_HOME", "h/gemini"),
    ] {
        c.env(k, d.join(sub));
    }
    c.env("VIBEKE_NOTIFIER", "none");
    c.env_remove("VIBEKE_ELEVATED_TOKEN");
    // SAFETY: umask is async-signal-safe.
    unsafe {
        c.pre_exec(|| {
            libc::umask(0o022);
            Ok(())
        });
    }
    c
}

fn json(s: &Session, args: &[&str]) -> Value {
    let out = cmd(s, args).output().unwrap();
    assert!(
        out.status.success(),
        "vibeke {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// Wait for a file the pane writes when its command finished.
fn wait_file(p: &Path, secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Ok(t) = std::fs::read_to_string(p)
            && t.ends_with('\n')
        {
            return t;
        }
        assert!(Instant::now() < deadline, "{} never written", p.display());
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Run `line` in `pane`; its stdout+stderr go to `<dir>/<tag>.out`, its exit code to
/// `<dir>/<tag>.rc` (written last). Returns (exit code, output).
fn in_pane(s: &Session, pane: &str, tag: &str, line: &str) -> (i32, String) {
    let d = s.dir.path();
    let (out, rc) = (d.join(format!("{tag}.out")), d.join(format!("{tag}.rc")));
    json(
        s,
        &[
            "pane",
            "run",
            pane,
            &format!(
                "{{ {line}; }} > {} 2>&1; echo $? > {}.tmp && mv {}.tmp {}",
                out.display(),
                rc.display(),
                rc.display(),
                rc.display()
            ),
        ],
    );
    let code = wait_file(&rc, 30).trim().parse().unwrap();
    (code, std::fs::read_to_string(out).unwrap_or_default())
}

fn wait_shell(s: &Session, pane: &str) {
    let _ = cmd(
        s,
        &[
            "pane",
            "wait-idle",
            pane,
            "--quiet-ms",
            "500",
            "--timeout-ms",
            "10000",
        ],
    )
    .output();
}

#[test]
fn unsafe_runtime_dir_is_refused() {
    let s = Session::new();
    let run = s.dir.path().join("run");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o777)).unwrap();
    let out = cmd(&s, &["server", "--foreground"]).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("group- or world-writable"), "{err}");
    assert!(!run.join("default/vibeke.sock").exists());
    assert_eq!(mode(&run), 0o777, "a refused dir is left as found");
}

#[test]
fn elevation_revocation_policy_audit_integrity_and_bundle() {
    let s = Session::new();
    let d = s.dir.path().to_path_buf();
    let ws = json(&s, &["workspace", "create", "--cwd", &d.to_string_lossy()]);
    let p = ws["root_pane"]["id"].as_str().unwrap().to_string();
    wait_shell(&s, &p);

    // 09 §3.1: the server's files are private, the pane keeps the user's umask.
    let state = d.join("state/default");
    assert_eq!(mode(&state.join("state.db")), 0o600);
    assert_eq!(mode(&d.join("run/default")), 0o700);
    assert_eq!(mode(&state.join("logs/server.log")), 0o600);
    let (code, out) = in_pane(&s, &p, "umask", "umask; touch f.txt; ls -l f.txt");
    assert_eq!(code, 0);
    assert!(out.starts_with("0022") || out.starts_with("022"), "{out}");
    assert!(
        out.contains("-rw-r--r--"),
        "pane files keep the user's mode: {out}"
    );

    // Policy is full scope only.
    let (code, out) = in_pane(&s, &p, "pl", "$VIBEKE_BIN --json policy list");
    assert_ne!(code, 0, "{out}");
    assert!(out.contains("permission_denied"), "{out}");

    // 09 §3.2: the pane asks for elevation; the user approves outside it.
    json(
        &s,
        &[
            "pane",
            "run",
            &p,
            &format!(
                "$VIBEKE_BIN --json auth elevate 'install deps' --timeout-ms 30000 > {0}/el.out 2>&1; echo $? > {0}/el.rc.tmp && mv {0}/el.rc.tmp {0}/el.rc",
                d.display()
            ),
        ],
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    let request = loop {
        let l = json(&s, &["auth", "list"]);
        if let Some(r) = l["pending"].as_array().and_then(|a| a.first()) {
            assert_eq!(r["pane"], p.as_str());
            assert_eq!(r["reason"], "install deps");
            break r["request"].as_str().unwrap().to_string();
        }
        assert!(Instant::now() < deadline, "no pending elevation request");
        std::thread::sleep(Duration::from_millis(50));
    };
    let dec = json(&s, &["auth", "decide", &request, "approve"]);
    assert_eq!(dec["decision"], "approved");
    assert_eq!(wait_file(&d.join("el.rc"), 30).trim(), "0");
    let el: Value =
        serde_json::from_str(&std::fs::read_to_string(d.join("el.out")).unwrap()).unwrap();
    let token = el["token"].as_str().expect("token").to_string();
    let (code, out) = in_pane(
        &s,
        &p,
        "pl2",
        &format!("VIBEKE_ELEVATED_TOKEN={token} $VIBEKE_BIN --json policy list"),
    );
    assert_eq!(code, 0, "elevated: {out}");
    assert!(out.contains("\"rules\""), "{out}");

    // Policy through the CLI.
    let added = json(
        &s,
        &[
            "policy",
            "add",
            "--tool",
            "Bash",
            "--command-regex",
            "^ls( |$)",
            "--effect",
            "allow",
        ],
    );
    let id = added["rule"]["id"].as_str().unwrap().to_string();
    let t = json(
        &s,
        &["policy", "test", "--tool", "Bash", "--command", "ls -la"],
    );
    assert_eq!(t["effect"], "allow", "{t}");
    assert_eq!(t["rule"]["id"], id.as_str());
    let t = json(
        &s,
        &["policy", "test", "--tool", "Bash", "--command", "rm x"],
    );
    assert_eq!(t["effect"], "ask");
    // A repository allow rule needs --allow-policy-grants.
    let repo = d.join("repo");
    std::fs::create_dir_all(repo.join(".vibeke")).unwrap();
    std::fs::write(
        repo.join(".vibeke/policy.toml"),
        "[[rule]]\nmatch = { tool = \"Edit\" }\neffect = \"allow\"\n",
    )
    .unwrap();
    let edit = [
        "policy",
        "test",
        "--tool",
        "Edit",
        "--path",
        "a.rs",
        "--scope",
        repo.to_str().unwrap(),
    ];
    json(&s, &["policy", "trust", repo.to_str().unwrap()]);
    assert_eq!(json(&s, &edit)["effect"], "ask");
    let tr = json(
        &s,
        &[
            "policy",
            "trust",
            repo.to_str().unwrap(),
            "--allow-policy-grants",
        ],
    );
    assert_eq!(tr["allow_policy_grants"], true);
    assert_eq!(json(&s, &edit)["effect"], "allow");
    let l = json(&s, &["policy", "list"]);
    assert!(
        l["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["source"] == "repo")
    );
    assert_eq!(json(&s, &["policy", "remove", &id])["removed"], true);

    // 09 §5.3: integration tamper detection against the redirected Claude config.
    let out = cmd(&s, &["integration", "install", "claude", "--yes"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc = json(
        &s,
        &[
            "api",
            "call",
            "integration.doctor",
            r#"{"harness":"claude"}"#,
        ],
    );
    assert_eq!(doc["checks"][0]["status"], "ok", "{doc}");
    let problems = vk_server::api_schema::validate_result("integration.doctor", &doc);
    assert!(problems.is_empty(), "{problems:?}");
    let settings = d.join("h/claude/settings.json");
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    v["hooks"] = serde_json::json!({});
    std::fs::write(&settings, v.to_string()).unwrap();
    let doc = json(
        &s,
        &[
            "api",
            "call",
            "integration.doctor",
            r#"{"harness":"claude"}"#,
        ],
    );
    assert_eq!(doc["checks"][0]["status"], "removed", "{doc}");
    assert_eq!(doc["checks"][0]["ok"], false);

    // 09 §9.5: the bundle never carries the pane token, even when the pane printed it.
    let (_, tok) = in_pane(&s, &p, "tok", "echo $VIBEKE_PANE_TOKEN");
    let pane_token = tok.trim().to_string();
    assert_eq!(pane_token.len(), 64, "{tok}");
    std::thread::sleep(Duration::from_millis(300));
    let bundle = d.join("bundle.tar");
    let b = json(
        &s,
        &[
            "debug",
            "bundle",
            "--out",
            bundle.to_str().unwrap(),
            "--include-pane",
            &p,
        ],
    );
    assert_eq!(b["path"], bundle.to_str().unwrap());
    let names: Vec<&str> = b["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    for n in [
        "db.json",
        "config.json",
        "doctor.txt",
        "server.json",
        "audit.json",
    ] {
        assert!(names.contains(&n), "{n}: {names:?}");
    }
    assert!(names.iter().any(|n| n.starts_with("panes/")), "{names:?}");
    assert!(!names.iter().any(|n| n.starts_with("scrollback/")));
    let bytes = std::fs::read(&bundle).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains(&pane_token), "pane token in the bundle");
    assert!(!text.contains(&token), "elevated token in the bundle");
    assert_eq!(mode(&bundle), 0o600);

    // 09 §3.2: revoking the pane's token cuts it off (and ends its elevation).
    let r = json(&s, &["pane", "revoke-token", &p]);
    assert_eq!(r["revoked"], true);
    let (code, out) = in_pane(&s, &p, "rv", "$VIBEKE_BIN --json pane list");
    assert_ne!(code, 0);
    assert!(out.contains("token_revoked"), "{out}");
    let (code, out) = in_pane(
        &s,
        &p,
        "rv2",
        &format!("VIBEKE_ELEVATED_TOKEN={token} $VIBEKE_BIN --json pane list"),
    );
    assert_ne!(code, 0, "elevation ended with the revocation: {out}");

    // 09 §11: all of it is in the hash-chained audit log, verified by doctor.
    let tail = json(&s, &["audit", "tail", "--limit", "200"]);
    let types: Vec<&str> = tail["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    for t in [
        "security.permission_denied",
        "auth.elevate_requested",
        "auth.elevate_granted",
        "policy.rule_added",
        "policy.repo_trusted",
        "policy.rule_removed",
        "auth.token_revoked",
    ] {
        assert!(types.contains(&t), "{t} not audited: {types:?}");
    }
    let found = json(&s, &["audit", "search", "install deps"]);
    assert!(!found["entries"].as_array().unwrap().is_empty());
    assert_eq!(json(&s, &["audit", "verify"])["ok"], true);
    let out = cmd(&s, &["doctor", "--audit"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let log: PathBuf = state.join("audit.jsonl");
    assert_eq!(mode(&log), 0o600);
    let text = std::fs::read_to_string(&log).unwrap();
    std::fs::write(&log, text.replacen("install deps", "nothing here", 1)).unwrap();
    let out = cmd(&s, &["doctor", "--audit"]).output().unwrap();
    assert!(!out.status.success(), "a rewritten entry fails doctor");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("does not match its hash"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}
