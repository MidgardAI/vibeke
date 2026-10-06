//! `vibeke forget` and `vibeke doctor --rebuild-index` against a real server in an isolated
//! VIBEKE_* session (02 "Archive search as implemented", 09 §9.3).

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkforget")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(dir.path().join("config.toml"), "").unwrap();
        Session { dir }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env("VIBEKE_TEST_HOOKS", "1");
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
            "VIBEKE_PANE_ID",
        ] {
            c.env_remove(k);
        }
        c.arg("--json").args(args);
        // No terminal on stdin: confirmations must not be answerable.
        c.stdin(std::process::Stdio::null());
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            self.log_tail()
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
    fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(self.path().join("state/default/logs/server.log"))
            .unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        lines[lines.len().saturating_sub(30)..].join("\n")
    }
    fn state(&self) -> PathBuf {
        self.path().join("state/default")
    }
    fn archive_hits(&self, q: &str) -> usize {
        let p = json!({"q": q, "sources": ["archive"], "limit": 100}).to_string();
        self.json(&["api", "call", "search.query", &p])["hits"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0)
    }
    fn wait_hits(&self, q: &str) {
        let t0 = Instant::now();
        while self.archive_hits(q) == 0 {
            assert!(
                t0.elapsed() < Duration::from_secs(20),
                "no archive hit for {q}\n{}",
                self.log_tail()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    /// A workspace whose shell prints `marker` and then enough lines to push it into history.
    fn workspace_with(&self, marker: &str) -> String {
        let p = json!({"cwd": "/tmp", "command": ["/bin/sh"]}).to_string();
        let v = self.json(&["api", "call", "workspace.create", &p]);
        let pane = v["root_pane"]["id"].as_str().unwrap().to_string();
        self.json(&[
            "pane",
            "run",
            &pane,
            &format!("echo {marker}; seq 1 300; echo end$((1+1))"),
        ]);
        self.json(&[
            "pane",
            "wait-output",
            &pane,
            "--regex",
            "end2",
            "--timeout-ms",
            "15000",
        ]);
        // Archive hits only show for rows that are no longer in a live pane's memory, so close
        // the pane: its history is then served from the archive alone.
        self.json(&["pane", "close", &pane]);
        v["workspace"]["id"].as_str().unwrap().to_string()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn forget_workspace_needs_confirmation_then_deletes_only_that_scope() {
    let s = Session::new();
    let w1 = s.workspace_with("needleone");
    let _w2 = s.workspace_with("needletwo");
    s.wait_hits("needleone");
    s.wait_hits("needletwo");

    // Scope is required; exactly one.
    assert_eq!(s.run(&["forget"]).status.code(), Some(2));
    assert_eq!(
        s.run(&["forget", "--all", "--pane", "x"]).status.code(),
        Some(2)
    );

    // --dry-run reports and deletes nothing.
    let dry = s.json(&["forget", "--workspace", &w1, "--dry-run"]);
    assert_eq!(dry["dry_run"], true);
    assert!(dry["fts_rows_deleted"].as_u64().unwrap() > 0, "{dry}");
    assert!(s.archive_hits("needleone") > 0);

    // Without --yes and without a terminal it asks and stops.
    let o = s.run(&["forget", "--workspace", &w1]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("rerun with --yes"), "{}", stderr(&o));
    assert!(
        s.archive_hits("needleone") > 0,
        "nothing deleted without --yes"
    );

    // With --yes only that workspace's archive goes.
    let done = s.json(&["forget", "--workspace", &w1, "--yes"]);
    assert_eq!(done["dry_run"], false);
    assert_eq!(done["fts_rows_deleted"], dry["fts_rows_deleted"]);
    assert!(done["segments_deleted"].as_u64().unwrap() >= 1);
    assert_eq!(s.archive_hits("needleone"), 0);
    assert!(s.archive_hits("needletwo") > 0);

    // Idempotent: nothing left, still success.
    let o = s.run(&["forget", "--workspace", &w1, "--yes"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stderr(&o).contains("nothing to forget"), "{}", stderr(&o));

    // The event log records scope and counts only.
    let ev = s.json(&[
        "api",
        "call",
        "events.read",
        &json!({"after": 0, "limit": 5000}).to_string(),
    ]);
    let text = ev.to_string();
    assert!(text.contains("scrollback.forgotten"), "{text}");
    assert!(!text.contains("needleone\\\""), "event must not carry text");

    // --all clears the rest.
    s.json(&["forget", "--all", "--yes"]);
    assert_eq!(s.archive_hits("needletwo"), 0);
}

#[test]
fn doctor_rebuild_index_refuses_while_running_then_rebuilds_offline() {
    let s = Session::new();
    s.workspace_with("needleone");
    s.wait_hits("needleone");

    // Refuses (and changes nothing) while the server runs.
    let o = s.run(&["doctor", "--rebuild-index"]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("server is running"), "{}", stderr(&o));

    s.json(&["server", "stop", "--kill-panes"]);
    let pid: i32 = std::fs::read_to_string(s.path().join("run/default/server.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let t0 = Instant::now();
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "server did not stop"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // Damage the derived index and add a corrupt segment for a pane that never existed.
    let db = s.state().join("state.db");
    {
        let store = vk_store::Store::open(&db).unwrap();
        store
            .fts_insert(&[("ghost".into(), 1, 1, "ghostrow".into())])
            .unwrap();
    }
    let bad = s.state().join("scrollback/zz-corrupt");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(bad.join("0000000000000000.zst"), b"definitely not zstd").unwrap();
    let before: Vec<_> = std::fs::read_dir(s.state().join("scrollback"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();

    let v = s.json(&["doctor", "--rebuild-index"]);
    let r = &v["rebuilt"];
    assert!(r["rows_indexed"].as_u64().unwrap() > 0, "{v}");
    assert_eq!(
        r["skipped"],
        json!(["zz-corrupt/0000000000000000.zst"]),
        "{v}"
    );
    assert!(r["fts_rows_before"].as_u64().unwrap() > 0);
    // Segment files are untouched, the corrupt one included.
    assert!(bad.join("0000000000000000.zst").exists());
    let after: Vec<_> = std::fs::read_dir(s.state().join("scrollback"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(before.len(), after.len());
    {
        let store = vk_store::Store::open(&db).unwrap();
        assert!(store.fts_search("ghostrow", None, 5).unwrap().is_empty());
        assert!(!store.fts_search("needleone", None, 5).unwrap().is_empty());
    }

    // The server comes back up on the rebuilt index and search works.
    s.wait_hits("needleone");
}
