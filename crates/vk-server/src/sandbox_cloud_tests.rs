//! Pure parts of the `cloud` level: argv shapes, error mapping, the unsynced report and the
//! reconciler's ownership rules.

use super::*;
use crate::cloud_reconcile::{OwnIn, ownership};
use vk_cloud::ErrorKind as CK;

fn runner(root: &Path) -> CloudRunner {
    CloudRunner {
        provider: "fake",
        box_ref: "fake/b1".into(),
        host_bin: "/opt/vibeke/bin/vibeke".into(),
        workdir: BOX_WORKSPACE.into(),
        root: root.to_path_buf(),
        run_dir: root.join("run"),
        box_root: String::new(),
        exec_env: vec![(
            "CLAUDE_CONFIG_DIR".into(),
            "/vibeke/creds/home/claude".into(),
        )],
        secrets: vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "sk-FAKE-secret".into())],
        cli_env: vec![("HOME".into(), "/Users/x".into())],
    }
}

#[test]
fn pane_argv_is_a_cloud_exec_with_identity_and_no_secrets() {
    let t = tempfile::tempdir().unwrap();
    let r = runner(t.path());
    let p = r
        .prepare(SpawnRequest {
            pane_id: "01PANE0000000000000000ABCD".into(),
            argv: vec!["/bin/zsh".into(), "-l".into()],
            cwd: "/host/co".into(),
            env: vec![
                ("VIBEKE_PANE_TOKEN".into(), "tok".into()),
                ("TERM".into(), "xterm-256color".into()),
                ("VIBEKE_PANE_ID".into(), "w1:p1".into()),
                ("HOME".into(), "/Users/x".into()),
            ],
        })
        .unwrap();
    let j = p.argv.join(" ");
    assert!(
        j.starts_with("/opt/vibeke/bin/vibeke cloud exec -i -t -w /workspace -e "),
        "{j}"
    );
    assert!(j.contains("-e CLAUDE_CONFIG_DIR=/vibeke/creds/home/claude"));
    assert!(j.contains("-e TERM=xterm-256color"));
    assert!(j.contains("-e VIBEKE_PANE_ID=w1:p1"));
    assert!(j.contains("-e VIBEKE_SOCKET=/tmp/vibeke-brokers/0000abcd.sock"));
    // Secrets go by name; the value is only in the holder's env.
    assert!(j.contains("-e CLAUDE_CODE_OAUTH_TOKEN --session-file "));
    assert!(!j.contains("sk-FAKE-secret"));
    assert!(!j.contains("VIBEKE_PANE_TOKEN"));
    assert!(
        p.env
            .iter()
            .any(|(k, v)| k == "CLAUDE_CODE_OAUTH_TOKEN" && v == "sk-FAKE-secret")
    );
    // The host login shell becomes the box's bash.
    assert!(j.ends_with("fake/b1 -- bash -l"), "{j}");
    assert!(j.contains(&format!(".ctl/0000abcd/{SESSION_FILE} fake/b1 -- ")));
    assert_eq!(p.broker_socket.unwrap(), t.path().join("run/0000abcd.sock"));
    assert!(p.visible_roots.is_empty());
}

#[test]
fn link_and_git_services_go_through_cloud_exec() {
    assert_eq!(
        link_argv("/b/vibeke", "sprites/vk-1", BOX_BIN, BOX_BROKERS),
        [
            "/b/vibeke",
            "cloud",
            "exec",
            "-i",
            "sprites/vk-1",
            "--",
            "/vibeke/bin/vibeke",
            "sandbox",
            "bridge",
            "--brokers",
            "/tmp/vibeke-brokers"
        ]
    );
    assert_eq!(
        git_service("/b/my vibeke", "e2b/abc", "receive-pack"),
        "'/b/my vibeke' cloud exec -i e2b/abc -- git receive-pack"
    );
}

#[test]
fn projection_env_splits_paths_from_secrets() {
    let shared = Path::new("/state/sbx/k/shared");
    let (env, secrets) = split_projection_env(
        &[
            (
                "CLAUDE_CONFIG_DIR".into(),
                "/state/sbx/k/shared/home/claude".into(),
            ),
            ("CLAUDE_CODE_OAUTH_TOKEN".into(), "sk-x".into()),
            ("OTHER".into(), "/state/sbx/k/sharedother".into()),
        ],
        shared,
        BOX_CREDS,
    );
    assert_eq!(
        env,
        [(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/vibeke/creds/home/claude".to_string()
        )]
    );
    assert_eq!(secrets.len(), 2);
}

#[test]
fn cli_env_keeps_only_host_basics() {
    let host: Vec<(String, String)> = [
        ("PATH", "/usr/bin"),
        ("HOME", "/Users/x"),
        ("ANTHROPIC_API_KEY", "sk-no"),
        ("VIBEKE_CLOUD_FAKE_DIR", "/tmp/f"),
        ("SPRITES_TOKEN", "org/x/y"),
        ("LC_ALL", "C"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let env = cli_env(&host, &["SPRITES_TOKEN".to_string()]);
    let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        [
            "PATH",
            "HOME",
            "VIBEKE_CLOUD_FAKE_DIR",
            "SPRITES_TOKEN",
            "LC_ALL"
        ]
    );
}

#[test]
fn identity_env_drops_the_pane_token() {
    let e = identity_env(&[
        ("VIBEKE_PANE_TOKEN".into(), "tok".into()),
        ("VIBEKE_PANE_ID".into(), "p".into()),
        ("LC_CTYPE".into(), "UTF-8".into()),
    ]);
    assert_eq!(e.len(), 2);
    assert!(e.iter().all(|(k, _)| k != "VIBEKE_PANE_TOKEN"));
}

#[test]
fn box_refs_parse() {
    assert_eq!(
        parse_box_ref("sprites/vk-ab-cd").unwrap(),
        ("sprites".to_string(), "vk-ab-cd".to_string())
    );
    for bad in ["sprites", "/x", "sprites/", "a/b/c"] {
        assert!(parse_box_ref(bad).is_err(), "{bad}");
    }
}

#[test]
fn uname_arch_is_normalized() {
    assert_eq!(
        parse_uname("Linux x86_64\n"),
        ("Linux".to_string(), "x86_64".to_string())
    );
    assert_eq!(parse_uname("Linux arm64").1, "aarch64");
    assert_eq!(parse_uname("Darwin arm64").0, "Darwin");
    assert!(
        linux_vibeke_path(Path::new("/h"), "aarch64")
            .to_string_lossy()
            .ends_with("vibeke-linux-aarch64")
    );
}

#[test]
fn provider_errors_map_to_api_kinds() {
    let methods = vec![AuthMethod::PasteToken {
        label: "Token".into(),
        help_url: "https://example.com/tokens".into(),
        hint: "".into(),
    }];
    let e = rpc_error(
        "fake",
        &methods,
        &CloudError::new(CK::NeedsAuth, "sign in to fake first"),
    );
    assert!(e.kind_is(ErrorKind::PermissionDenied));
    assert_eq!(e.data.details["reason"], "needs_auth");
    assert_eq!(e.data.details["provider"], "fake");
    assert_eq!(e.data.details["methods"][0]["kind"], "paste_token");
    let e = rpc_error("fake", &methods, &CloudError::new(CK::Account, "quota"));
    assert!(e.kind_is(ErrorKind::PermissionDenied));
    assert_eq!(e.data.details["reason"], "account");
    for (k, want) in [
        (CK::NotFound, ErrorKind::NotFound),
        (CK::Conflict, ErrorKind::Conflict),
        (CK::Unsupported, ErrorKind::Unsupported),
        (CK::RateLimited, ErrorKind::RateLimited),
        (CK::InvalidParams, ErrorKind::InvalidParams),
        (CK::Unavailable, ErrorKind::RemoteUnavailable),
        (CK::Internal, ErrorKind::Internal),
    ] {
        assert!(
            rpc_error("fake", &methods, &CloudError::new(k, "m")).kind_is(want),
            "{k:?}"
        );
    }
}

#[test]
fn unsynced_report_parses_and_combines_with_the_host() {
    let r = parse_report(
        "head=0123456789abcdef0123456789abcdef01234567\nahead=2\ndirty=       3\nuntracked=1\nstashes=0\n",
    );
    assert!(!r.missing);
    assert_eq!(r.ahead, 2);
    assert_eq!(r.dirty, 3);
    assert_eq!(r.untracked, 1);
    // The host has every commit (pulled): only the worktree counts.
    let u = combine(&r, Some(Some(0)));
    assert_eq!((u.commits, u.dirty, u.untracked), (0, 3, 1));
    assert!(!u.is_clean());
    // The host lacks the head commit: at least one commit is only in the box.
    let clean = parse_report("head=abc\nahead=0\ndirty=0\nuntracked=0\nstashes=0\n");
    assert_eq!(combine(&clean, Some(None)).commits, 1);
    assert!(combine(&clean, Some(Some(0))).is_clean());
    assert_eq!(combine(&clean, Some(Some(0))).summary, "clean");
    // An empty repo has nothing to lose.
    let empty = parse_report("head=\nahead=0\ndirty=0\nuntracked=0\nstashes=0\n");
    assert!(combine(&empty, None).is_clean());
    assert!(parse_report("missing=1\n").missing);
    // A missing or unreadable repository is unknown, never clean, and says so additively.
    let u = Unsynced::unknown("the box repository is missing");
    assert!(!u.is_clean());
    assert_eq!(serde_json::to_value(&u).unwrap()["unknown"], true);
    let clean = Unsynced::from_counts(0, 0, 0, 0);
    assert!(
        serde_json::to_value(&clean)
            .unwrap()
            .get("unknown")
            .is_none()
    );
    // Stashes count as dirty and are named in the summary.
    let s = Unsynced::from_counts(1, 0, 2, 1);
    assert_eq!(s.dirty, 1);
    assert_eq!(
        s.summary,
        "1 commit, 1 stash, 2 untracked files not on the host"
    );
}

#[test]
fn other_branches_and_submodules_count_as_unsynced() {
    let r =
        parse_report("head=abc\nahead=0\nother=2\nsubmodules=1\ndirty=0\nuntracked=0\nstashes=0\n");
    assert_eq!((r.other, r.submodules), (2, 1));
    let u = combine(&r, Some(Some(0)));
    assert!(!u.is_clean());
    assert_eq!((u.commits, u.dirty), (2, 1));
    assert_eq!(
        u.summary,
        "2 commits on other branches or tags, 1 changed submodule not on the host"
    );
    // The script asks for both, outside the synced-tree check.
    let s = unsynced_script("/workspace", Some("main"), Some("abc"));
    assert!(s.contains("--branches --tags --not"), "{s}");
    assert!(s.contains("refs/vibeke/host/*"), "{s}");
    assert!(s.contains("echo \"other=$o\""), "{s}");
    assert!(s.contains("echo \"submodules=$sm\""), "{s}");
    let mark = s.find(SYNCED_MARK).unwrap();
    assert!(s.find("submodules=$sm").unwrap() < mark, "{s}");
}

#[test]
fn scripts_quote_branches() {
    let s = init_script("/workspace", "vk/it's");
    assert!(s.contains("receive.denyCurrentBranch updateInstead"));
    assert!(s.contains(r"'refs/heads/vk/it'\''s'"));
    let c = checkout_script("/workspace", "main", Some("A B"), None);
    assert!(c.contains("update-ref refs/heads/main refs/vibeke/host/main"));
    assert!(c.contains("config user.name 'A B'"));
    assert!(!c.contains("user.email"));
    let u = unsynced_script("/workspace", Some("main"), Some("abc"));
    assert!(u.contains("r=refs/vibeke/host/main; b=abc"));
    assert!(unsynced_script("/workspace", None, None).contains("r=''; b=''"));
}

#[test]
fn ownership_rules() {
    let base = OwnIn {
        our_host: "aaaaaaaa",
        box_host: Some("aaaaaaaa"),
        listed: true,
        ours: true,
        task_exists: true,
        live: true,
        idle_for_s: 0,
        idle_after_s: Some(1800),
    };
    assert_eq!(ownership(&base), "attached");
    assert_eq!(
        ownership(&OwnIn {
            listed: false,
            ..base
        }),
        "missing"
    );
    assert_eq!(
        ownership(&OwnIn {
            box_host: Some("bbbbbbbb"),
            ..base
        }),
        "foreign"
    );
    assert_eq!(
        ownership(&OwnIn {
            ours: false,
            ..base
        }),
        "orphaned"
    );
    assert_eq!(
        ownership(&OwnIn {
            task_exists: false,
            ..base
        }),
        "orphaned"
    );
    let quiet = OwnIn {
        live: false,
        idle_for_s: 100,
        ..base
    };
    assert_eq!(ownership(&quiet), "attached");
    assert_eq!(
        ownership(&OwnIn {
            idle_for_s: 1800,
            ..quiet
        }),
        "idle"
    );
    assert_eq!(
        ownership(&OwnIn {
            idle_for_s: 99_999,
            idle_after_s: None,
            ..quiet
        }),
        "attached"
    );
}

#[test]
fn records_round_trip_without_secrets() {
    let r = BoxRecord {
        provider: "fake".into(),
        id: "b1".into(),
        key: "T1".into(),
        task: Some("T1".into()),
        unsynced: Some(Unsynced::from_counts(0, 0, 0, 0)),
        ..Default::default()
    };
    assert_eq!(r.box_ref(), "fake/b1");
    assert!(r.ours());
    let s = serde_json::to_string(&r).unwrap();
    let back: BoxRecord = serde_json::from_str(&s).unwrap();
    assert_eq!(back, r);
    // Older or partial records still load.
    let partial: BoxRecord = serde_json::from_str(r#"{"provider":"e2b","id":"x"}"#).unwrap();
    assert!(!partial.ours());
}

#[test]
fn only_management_sessions_leave_a_box_idle() {
    let s = |command: &str, tty: bool, active: bool| vk_cloud::SessionInfo {
        id: "1".into(),
        command: command.into(),
        tty,
        active,
        last_activity_at: 0,
    };
    assert!(user_session(&s("bash -l", true, true)));
    assert!(!user_session(&s("bash -l", true, false)));
    // A batch command without a terminal (`vibeke cloud exec box -- make test`) is use.
    assert!(user_session(&s("make test", false, true)));
    assert!(user_session(&s("python3 train.py --epochs 3", false, true)));
    // Vibeke's own sessions are not: the bridge, the bundle commands, git services, scripts.
    for cmd in [
        "/vibeke/bin/vibeke sandbox bridge --brokers /tmp/vibeke-brokers",
        "/vibeke/bin/vibeke sandbox bridge",
        "/tmp/fake-root/vibeke/bin/vibeke sandbox import-bundle --bundle /vibeke/in/x.tar.zst",
        "git upload-pack /workspace",
        "git receive-pack '/workspace'",
        "/usr/bin/git git-upload-pack /workspace",
    ] {
        assert!(!user_session(&s(cmd, false, true)), "{cmd}");
        assert!(!user_session(&s(cmd, true, true)), "{cmd}");
    }
    // The server's scripts carry the marker in their command.
    let argv = mgmt_argv(&["sh".into(), "-c".into(), "git status".into()]);
    assert_eq!(argv[0], "/bin/sh");
    assert!(!user_session(&s(&argv.join(" "), false, true)));
    assert_eq!(&argv[4..], &["sh", "-c", "git status"]);
    // Marked once.
    assert_eq!(mgmt_argv(&argv), argv);
    // A user's git or vibeke commands are use.
    assert!(user_session(&s("git log", false, true)));
    assert!(user_session(&s("vibeke sandbox bridge", false, true)));
}

#[test]
fn marked_commands_keep_their_exit_code_and_input() {
    let argv = mgmt_argv(&[
        "sh".into(),
        "-c".into(),
        "read x; echo \"got $x\"; exit 7".into(),
    ]);
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(b"hi\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "got hi\n");
}

#[test]
fn notes_commits_count_as_unsynced() {
    let r = parse_report("head=abc\nahead=0\nother=0\nnotes=2\ndirty=0\nuntracked=0\nstashes=0\n");
    assert_eq!(r.notes, 2);
    let u = combine(&r, Some(Some(0)));
    assert!(!u.is_clean());
    assert_eq!(u.commits, 2);
    assert_eq!(u.summary, "2 notes commits not on the host");
    // The script, run in a real repository: notes no host ref reaches are counted.
    let dir = tempfile::tempdir().unwrap();
    let w = dir.path();
    let git = |args: &[&str]| {
        let o = std::process::Command::new("git")
            .arg("-C")
            .arg(w)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["commit", "-q", "--allow-empty", "-m", "one"]);
    let run = || {
        let script = unsynced_script(&w.to_string_lossy(), None, None);
        let o = std::process::Command::new("sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        parse_report(&String::from_utf8_lossy(&o.stdout))
    };
    assert_eq!(run().notes, 0);
    git(&["notes", "add", "-m", "a note", "HEAD"]);
    git(&["notes", "add", "-f", "-m", "another", "HEAD"]);
    assert_eq!(run().notes, 2);
    // Once a host ref reaches them, they are on the host.
    git(&["update-ref", "refs/vibeke/host/notes", "refs/notes/commits"]);
    assert_eq!(run().notes, 0);
}

#[test]
fn records_remember_recent_panes() {
    let mut last = vec![];
    for p in ["a", "b", "a"] {
        push_last_pane(&mut last, p);
    }
    assert_eq!(last, vec!["a".to_string(), "b".to_string()]);
    for i in 0..40 {
        push_last_pane(&mut last, &format!("p{i}"));
    }
    assert_eq!(last.len(), LAST_PANES);
    assert_eq!(last[0], "p39");
    // Older records load without the new fields.
    let r: BoxRecord = serde_json::from_str(r#"{"provider":"e2b","id":"x"}"#).unwrap();
    assert!(!r.held && r.last_panes.is_empty());
}

#[test]
fn box_link_serves_old_broker_names_for_the_new_pane() {
    let link = Arc::new(crate::sandbox::container::BoxLink::default());
    let pane = "01HZZZZZZZZZZZZZZZZZZZZZZZ";
    let short = vk_sandbox::runner::short_id(pane);
    link.add_pane(pane);
    assert!(link.add_alias("oldpane1", pane));
    assert_eq!(link.resolve("oldpane1"), short);
    assert_eq!(link.resolve("other"), "other");
    // Never a live pane's own name, and only plain ids.
    let other = "01HYYYYYYYYYYYYYYYYYYYYYYY";
    link.add_pane(other);
    assert!(!link.add_alias(&vk_sandbox::runner::short_id(other), pane));
    assert!(!link.add_alias("../x", pane));
    // The alias goes with the pane that answers for it.
    link.remove_pane(pane);
    assert_eq!(link.resolve("oldpane1"), "oldpane1");
}

/// A reconcile pass that read a record before a destroy dropped it must not write it back
/// (it would come back as a `missing` box).
#[test]
fn an_update_never_brings_back_a_dropped_record() {
    let dir = tempfile::Builder::new()
        .prefix("vkcr")
        .tempdir_in("/tmp")
        .unwrap();
    let srv = crate::hardening::testkit::server(dir.path(), "cr");
    let rec = BoxRecord {
        provider: "fake".into(),
        id: "b1".into(),
        name: "b1".into(),
        state: "running".into(),
        ..Default::default()
    };
    save_record(&srv, &rec);
    let mut seen = load_record(&srv, "fake/b1").unwrap();
    seen.sessions = 2;
    assert!(update_record(&srv, &seen));
    assert_eq!(load_record(&srv, "fake/b1").unwrap().sessions, 2);
    drop_record(&srv, &rec);
    assert!(!update_record(&srv, &seen));
    assert!(load_record(&srv, "fake/b1").is_none());
}
