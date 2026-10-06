//! `vibeke doctor --list-backups` / `--restore-backup` (02 §3): offline, against a state directory
//! built in a temp dir; no server, no real state. The session's default name is `default`.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkbackup")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(dir.path().join("config.toml"), "").unwrap();
        Env { dir }
    }
    fn db(&self) -> PathBuf {
        self.dir.path().join("state/default/state.db")
    }
    fn run(&self, args: &[&str]) -> Output {
        let d: &Path = self.dir.path();
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
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
        c.arg("--json")
            .args(args)
            .stdin(std::process::Stdio::null());
        c.output().unwrap()
    }
    fn json(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
}

fn note(s: &mut vk_store::Store, id: &str) {
    let mut m = vk_store::Mutation::new();
    m.put("note", id, None, &json!({"id": id}));
    m.event("note.put", json!({}), json!({"id": id}));
    s.commit(m).unwrap();
}

#[test]
fn list_is_empty_for_a_fresh_session() {
    let e = Env::new();
    let v = e.json(&["doctor", "--list-backups"]);
    assert_eq!(v["backups"], json!([]));
    assert_eq!(v["keep"], 3);
}

#[test]
fn a_migration_leaves_a_backup_that_restore_puts_back_with_a_new_epoch() {
    let e = Env::new();
    let db = e.db();
    // A database one schema version behind, with content: the store backs it up when it opens
    // and migrates it.
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER);",
        )
        .unwrap();
        // Version 1 schema only: the first migration's tables.
        c.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE events (seq INTEGER PRIMARY KEY, ts INTEGER NOT NULL, type TEXT NOT NULL, tier TEXT NOT NULL,
               subject_json TEXT, actor_json TEXT, data_json TEXT NOT NULL, v INTEGER NOT NULL);
             CREATE INDEX events_tier_ts ON events(tier, ts);
             CREATE INDEX events_type_ts ON events(type, ts);
             CREATE TABLE entities (kind TEXT NOT NULL, id TEXT NOT NULL, handle TEXT, json TEXT NOT NULL,
               closed INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL, PRIMARY KEY (kind, id));
             CREATE INDEX entities_handle ON entities(kind, handle);
             CREATE TABLE vt_snapshots (pane_id TEXT PRIMARY KEY, holder_offset INTEGER NOT NULL, engine TEXT NOT NULL,
               engine_version TEXT NOT NULL, blob BLOB NOT NULL, taken_at INTEGER NOT NULL);
             CREATE TABLE holders (pane_id TEXT PRIMARY KEY, socket TEXT NOT NULL, key BLOB NOT NULL, epoch INTEGER NOT NULL,
               holder_pid INTEGER, child_pid INTEGER, updated_at INTEGER NOT NULL);
             CREATE TABLE pane_reads (user TEXT NOT NULL, pane_id TEXT NOT NULL, seen_rev INTEGER NOT NULL,
               seen_at INTEGER NOT NULL, PRIMARY KEY (user, pane_id));
             CREATE TABLE kv (scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (scope, key));
             CREATE VIRTUAL TABLE scrollback_fts USING fts5(pane_id UNINDEXED, line_no UNINDEXED, ts UNINDEXED, text);
             INSERT INTO schema_migrations VALUES (1, 0);
             INSERT INTO kv VALUES ('t', 'k', 'before-migration');",
        )
        .unwrap();
    }
    let epoch_before;
    {
        let mut s = vk_store::Store::open(&db).unwrap();
        epoch_before = s.log_epoch.clone();
        note(&mut s, "after-migration");
    }
    let v = e.json(&["doctor", "--list-backups"]);
    let list = v["backups"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{v}");
    assert_eq!(list[0]["schema_version"], 1);
    let name = list[0]["name"].as_str().unwrap().to_string();

    // Restoring needs a name and refuses nothing it should accept.
    let bad = e.run(&["doctor", "--restore-backup"]);
    assert_eq!(bad.status.code(), Some(2));
    let unknown = e.run(&["doctor", "--restore-backup", "state-v9-1.db"]);
    assert!(!unknown.status.success());

    let r = e.json(&["doctor", "--restore-backup", &name]);
    assert_eq!(r["log_epoch_rotated"], true);
    assert!(db.with_extension("db.pre-restore").exists());
    // The restored database is the pre-migration one: opening it migrates forward again, the
    // post-migration note is gone, and the epoch is new.
    let s = vk_store::Store::open(&db).unwrap();
    let notes: Vec<Value> = s.load("note").unwrap();
    assert!(notes.is_empty(), "state is the backup's");
    assert_eq!(
        s.kv_get("t", "k").unwrap().as_deref(),
        Some("before-migration")
    );
    assert_ne!(s.log_epoch, epoch_before);
}
