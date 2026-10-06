//! Persistent state for one Vibeke session (02 §3): SQLite (WAL) state tables that are the source
//! of truth, plus the transactional event outbox. Every [`Mutation`] commits entity writes and
//! its events in one transaction; a failed commit means the mutation did not happen (02 §4a).
//!
//! Entities are stored as JSON documents keyed by kind and id (with a handle column for
//! lookups). The typed schema of 02 §3 lives in `vk-proto::model`; storing documents keeps
//! migrations additive while the model is young.

pub mod archive;
pub mod backup;
pub mod blobs;
pub mod conv;
mod plugin_kv;
pub use plugin_kv::{
    DEFAULT_QUOTA as PLUGIN_KV_QUOTA, KvError, MAX_VALUE as PLUGIN_KV_MAX_VALUE, PluginCommand,
};
pub mod crypt;
pub mod keychain;
mod purge;
mod tombstone;
pub use purge::{PurgeRecovery, PurgeReport, RebuildReport};
pub use tombstone::{EventScope, TombstoneReport};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    r#"
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE events (
        seq INTEGER PRIMARY KEY, ts INTEGER NOT NULL, type TEXT NOT NULL, tier TEXT NOT NULL,
        subject_json TEXT, actor_json TEXT, data_json TEXT NOT NULL, v INTEGER NOT NULL);
    CREATE INDEX events_tier_ts ON events(tier, ts);
    CREATE INDEX events_type_ts ON events(type, ts);
    CREATE TABLE entities (
        kind TEXT NOT NULL, id TEXT NOT NULL, handle TEXT, json TEXT NOT NULL,
        closed INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL,
        PRIMARY KEY (kind, id));
    CREATE INDEX entities_handle ON entities(kind, handle);
    CREATE TABLE vt_snapshots (
        pane_id TEXT PRIMARY KEY, holder_offset INTEGER NOT NULL, engine TEXT NOT NULL,
        engine_version TEXT NOT NULL, blob BLOB NOT NULL, taken_at INTEGER NOT NULL);
    CREATE TABLE holders (
        pane_id TEXT PRIMARY KEY, socket TEXT NOT NULL, key BLOB NOT NULL, epoch INTEGER NOT NULL,
        holder_pid INTEGER, child_pid INTEGER, updated_at INTEGER NOT NULL);
    CREATE TABLE pane_reads (user TEXT NOT NULL, pane_id TEXT NOT NULL, seen_rev INTEGER NOT NULL,
        seen_at INTEGER NOT NULL, PRIMARY KEY (user, pane_id));
    CREATE TABLE kv (scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (scope, key));
    CREATE VIRTUAL TABLE scrollback_fts USING fts5(pane_id UNINDEXED, line_no UNINDEXED, ts UNINDEXED, text);
    "#,
    // 2: bind VT snapshots to the holder incarnation they were taken from (01 §1.2): a
    // snapshot is only valid for the holder process whose ring its offset refers to.
    r#"
    ALTER TABLE vt_snapshots ADD COLUMN holder_incarnation TEXT;
    "#,
    // 3: per-task lookups of task-scoped entities (check runs, grants, end candidates…)
    // without loading a kind's whole history (15 §11).
    r#"
    CREATE INDEX entities_task ON entities(kind, json_extract(json, '$.task'));
    "#,
    // 4: which workspace/pane an archived scrollback row belonged to, so archive search can
    // apply read scope (09 §5.1 rule 4) and label hits of panes that have since closed.
    r#"
    CREATE TABLE archive_panes (pane_id TEXT PRIMARY KEY, workspace TEXT, tab TEXT,
        handle TEXT, title TEXT, updated_at INTEGER NOT NULL);
    "#,
    // 5: approval policy rules added through the API (`policy.add`, 07 §2.9, 09 §4). Rules from
    // `config.toml` and trusted repository `.vibeke/policy.toml` files are read from their
    // files; this table holds only the rules the user added at runtime. `ord` keeps insertion
    // order (the first matching rule applies).
    r#"
    CREATE TABLE policy_rules (id TEXT PRIMARY KEY, ord INTEGER NOT NULL, rule_json TEXT NOT NULL,
        created_at INTEGER NOT NULL, created_by TEXT);
    "#,
    // 6: native plugins (07 §7.5, 02 §3 "Plugin state ownership"): the per-plugin KV namespace and
    // the session's native plugin command records (`plugin_kv.rs`). Self-contained: no other
    // migration depends on it.
    r#"
    CREATE TABLE IF NOT EXISTS plugin_kv (plugin_id TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL,
        updated_at INTEGER NOT NULL, PRIMARY KEY (plugin_id, key));
    CREATE TABLE IF NOT EXISTS plugin_commands (id TEXT PRIMARY KEY, plugin_id TEXT NOT NULL,
        status TEXT NOT NULL, started_at INTEGER NOT NULL, ended_at INTEGER, json TEXT NOT NULL);
    CREATE INDEX IF NOT EXISTS plugin_commands_plugin ON plugin_commands(plugin_id, started_at);
    "#,
];

/// Event-log retention (02 §2.3): `events.sync_retention`, `events.history_retention` and the
/// row cap (`events.max_rows`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub sync_days: i64,
    pub history_days: i64,
    /// Hard cap on the number of event rows; `0` disables it.
    pub max_rows: i64,
}

impl Default for Retention {
    fn default() -> Self {
        Retention {
            sync_days: 7,
            history_days: 365,
            max_rows: 2_000_000,
        }
    }
}

/// What one [`Store::prune_with`] pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct PruneReport {
    /// Rows removed for age.
    pub aged: usize,
    /// Rows removed to get under the row cap.
    pub capped: usize,
}

/// One row of `policy_rules` (migration 5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyRuleRow {
    pub id: String,
    pub ord: i64,
    pub rule: Value,
    pub created_at: i64,
    pub created_by: Option<String>,
}

/// One archived-scrollback search hit (`search.query`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FtsHit {
    pub pane: String,
    pub line: u64,
    pub ts: i64,
    pub text: String,
    pub workspace: Option<String>,
    pub handle: Option<String>,
    pub title: Option<String>,
}

/// Filters for [`Store::fts_query`]; `None` = unrestricted.
#[derive(Debug, Clone, Default)]
pub struct FtsQuery {
    pub q: String,
    pub panes: Option<Vec<String>>,
    pub workspaces: Option<Vec<String>>,
    pub since_ms: Option<i64>,
    pub limit: usize,
}

/// Pane identity recorded for archived rows: (pane, workspace, tab, handle, title).
pub type ArchivePane = (String, String, String, String, String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cursor {
    pub machine_uuid: String,
    pub session_uuid: String,
    pub log_epoch: String,
    pub seq: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub seq: i64,
    pub ts: i64,
    pub v: u32,
    pub tier: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub subject: Value,
    pub actor: Value,
    pub data: Value,
}

#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub kind: String,
    pub tier: &'static str,
    pub subject: Value,
    pub actor: Value,
    pub data: Value,
}

enum Write {
    Put {
        kind: &'static str,
        id: String,
        handle: Option<String>,
        json: String,
        closed: bool,
    },
    Delete {
        kind: &'static str,
        id: String,
    },
    Holder {
        pane: String,
        socket: String,
        key: Vec<u8>,
        epoch: u64,
        holder_pid: Option<u32>,
        child_pid: Option<u32>,
    },
    HolderDelete {
        pane: String,
    },
    Snapshot {
        pane: String,
        offset: u64,
        engine: String,
        version: String,
        blob: Vec<u8>,
        incarnation: String,
    },
    SnapshotDelete {
        pane: String,
    },
    Kv {
        scope: String,
        key: String,
        value: Option<String>,
    },
    Read {
        user: String,
        pane: String,
        rev: u64,
    },
    PolicyRule {
        id: String,
        /// `None` deletes the rule.
        rule: Option<(String, Option<String>)>,
    },
}

/// A set of state writes plus the events describing them, committed atomically.
#[derive(Default)]
pub struct Mutation {
    writes: Vec<Write>,
    pub events: Vec<PendingEvent>,
}

impl Mutation {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn is_empty(&self) -> bool {
        self.writes.is_empty() && self.events.is_empty()
    }
    pub fn put<T: Serialize>(
        &mut self,
        kind: &'static str,
        id: &str,
        handle: Option<&str>,
        value: &T,
    ) -> &mut Self {
        let json = serde_json::to_string(value).expect("entity serializes");
        self.writes.push(Write::Put {
            kind,
            id: id.into(),
            handle: handle.map(Into::into),
            json,
            closed: false,
        });
        self
    }
    /// Keep the row but mark it closed (history; excluded from `load`).
    pub fn close<T: Serialize>(
        &mut self,
        kind: &'static str,
        id: &str,
        handle: Option<&str>,
        value: &T,
    ) -> &mut Self {
        let json = serde_json::to_string(value).expect("entity serializes");
        self.writes.push(Write::Put {
            kind,
            id: id.into(),
            handle: handle.map(Into::into),
            json,
            closed: true,
        });
        self
    }
    pub fn delete(&mut self, kind: &'static str, id: &str) -> &mut Self {
        self.writes.push(Write::Delete {
            kind,
            id: id.into(),
        });
        self
    }
    pub fn holder(
        &mut self,
        pane: &str,
        socket: &str,
        key: &[u8],
        epoch: u64,
        holder_pid: Option<u32>,
        child_pid: Option<u32>,
    ) -> &mut Self {
        self.writes.push(Write::Holder {
            pane: pane.into(),
            socket: socket.into(),
            key: key.to_vec(),
            epoch,
            holder_pid,
            child_pid,
        });
        self
    }
    pub fn holder_delete(&mut self, pane: &str) -> &mut Self {
        self.writes.push(Write::HolderDelete { pane: pane.into() });
        self
    }
    /// Store a VT snapshot taken at holder ring `offset`; `incarnation` identifies the holder
    /// process whose ring that offset refers to (see `SnapshotRecord::incarnation`).
    pub fn snapshot(
        &mut self,
        pane: &str,
        offset: u64,
        engine: &str,
        version: &str,
        blob: Vec<u8>,
        incarnation: &str,
    ) -> &mut Self {
        self.writes.push(Write::Snapshot {
            pane: pane.into(),
            offset,
            engine: engine.into(),
            version: version.into(),
            blob,
            incarnation: incarnation.into(),
        });
        self
    }
    /// Forget a pane's VT snapshot (its holder was replaced; the old screen must not return).
    pub fn snapshot_delete(&mut self, pane: &str) -> &mut Self {
        self.writes
            .push(Write::SnapshotDelete { pane: pane.into() });
        self
    }
    pub fn kv(&mut self, scope: &str, key: &str, value: Option<String>) -> &mut Self {
        self.writes.push(Write::Kv {
            scope: scope.into(),
            key: key.into(),
            value,
        });
        self
    }
    pub fn read_mark(&mut self, user: &str, pane: &str, rev: u64) -> &mut Self {
        self.writes.push(Write::Read {
            user: user.into(),
            pane: pane.into(),
            rev,
        });
        self
    }
    /// Insert or replace an API-added policy rule (`policy_rules`); keeps the row's `ord` on
    /// replace, appends otherwise.
    pub fn policy_rule_put(
        &mut self,
        id: &str,
        rule: &Value,
        created_by: Option<&str>,
    ) -> &mut Self {
        self.writes.push(Write::PolicyRule {
            id: id.into(),
            rule: Some((rule.to_string(), created_by.map(str::to_string))),
        });
        self
    }
    pub fn policy_rule_delete(&mut self, id: &str) -> &mut Self {
        self.writes.push(Write::PolicyRule {
            id: id.into(),
            rule: None,
        });
        self
    }
    pub fn event(&mut self, kind: &str, subject: Value, data: Value) -> &mut Self {
        self.event_by(kind, subject, serde_json::json!({"kind": "system"}), data)
    }
    pub fn event_by(&mut self, kind: &str, subject: Value, actor: Value, data: Value) -> &mut Self {
        let tier = if kind.starts_with("interaction.")
            || kind.starts_with("policy.")
            // 09 §11: security-relevant records are history.
            || kind.starts_with("audit.")
            || kind.starts_with("auth.")
            || matches!(
                kind,
                "agent.started"
                    | "agent.exited"
                    | "task.status_changed"
                    | "task.archived"
                    // 15 §10.3: terminal check outcomes, acceptance and invalidation are history.
                    | "review.accepted"
                    | "review.invalidated"
                    | "check.passed"
                    | "check.failed"
                    | "check.cancelled"
                    | "check.interrupted"
                    | "check.unknown"
                    // 15 T4: dependency changes are history (§10.3).
                    | "task.dependency_changed"
                    // 02 §2.1: the history tier also holds sandbox boundary actions and
                    // plugin installs.
                    | "sandbox.boundary_action"
                    | "plugin.installed"
            ) {
            "history"
        } else {
            "sync"
        };
        self.events.push(PendingEvent {
            kind: kind.into(),
            tier,
            subject,
            actor,
            data,
        });
        self
    }
}

#[derive(Debug, Clone)]
pub struct HolderRecord {
    pub pane: String,
    pub socket: String,
    pub key: Vec<u8>,
    pub epoch: u64,
    pub holder_pid: Option<u32>,
    pub child_pid: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct SnapshotRecord {
    pub offset: u64,
    pub engine: String,
    pub version: String,
    pub blob: Vec<u8>,
    /// Holder incarnation the snapshot belongs to (`"<child_pid>:<holder started_at_ms>"`);
    /// `None` for snapshots written before migration 2, which are never trusted.
    pub incarnation: Option<String>,
}

pub struct Store {
    conn: Connection,
    pub machine_uuid: String,
    pub session_uuid: String,
    pub log_epoch: String,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        let s = Self::init(conn, path.parent(), Some(path))?;
        // 09 §3.1: state.db and its WAL files are 0600 whatever the umask was when they were
        // created (older versions created them with the default umask).
        restrict_file(path);
        for ext in ["-wal", "-shm"] {
            let mut p = path.as_os_str().to_owned();
            p.push(ext);
            restrict_file(Path::new(&p));
        }
        Ok(s)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, None, None)
    }

    fn init(conn: Connection, dir: Option<&Path>, db: Option<&Path>) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER)")?;
        let have: i64 = conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |r| r.get(0),
        )?;
        // A forward-only migration is never run without a way back (02 §3): an existing
        // database that is about to change schema is copied first, and a failed copy stops the
        // migration instead of running it unprotected.
        if let Some(db) = db
            && have > 0
            && (have as usize) < MIGRATIONS.len()
        {
            backup::make_backup(&conn, db, have)
                .context("pre-migration backup of state.db failed; not migrating")?;
        }
        for (i, m) in MIGRATIONS.iter().enumerate().skip(have as usize) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(m)?;
            tx.execute(
                "INSERT INTO schema_migrations VALUES (?1, ?2)",
                params![i as i64 + 1, now_ms()],
            )?;
            tx.commit()?;
        }
        let mut s = Store {
            conn,
            machine_uuid: String::new(),
            session_uuid: String::new(),
            log_epoch: String::new(),
        };
        s.session_uuid = s.meta_or_init("session_uuid", || ulid::Ulid::new().to_string())?;
        s.log_epoch = s.meta_or_init("log_epoch", || format!("{:016x}", rand::random::<u64>()))?;
        s.machine_uuid = machine_uuid(dir)?;
        Ok(s)
    }

    fn meta_or_init(&self, key: &str, f: impl FnOnce() -> String) -> Result<String> {
        if let Some(v) = self
            .conn
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .optional()?
        {
            return Ok(v);
        }
        let v = f();
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)",
            params![key, v],
        )?;
        Ok(v)
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=?2",
            params![key, value],
        )?;
        Ok(())
    }

    /// Commit writes and events in one transaction. Returns the committed events (with seq).
    pub fn commit(&mut self, m: Mutation) -> Result<Vec<Event>> {
        let tx = self.conn.transaction()?;
        let now = now_ms();
        for w in m.writes {
            match w {
                Write::Put {
                    kind,
                    id,
                    handle,
                    json,
                    closed,
                } => {
                    tx.execute(
                        "INSERT INTO entities (kind, id, handle, json, closed, updated_at) VALUES (?1,?2,?3,?4,?5,?6)
                         ON CONFLICT(kind, id) DO UPDATE SET handle=?3, json=?4, closed=?5, updated_at=?6",
                        params![kind, id, handle, json, closed as i64, now],
                    )?;
                }
                Write::Delete { kind, id } => {
                    tx.execute(
                        "DELETE FROM entities WHERE kind=?1 AND id=?2",
                        params![kind, id],
                    )?;
                }
                Write::Holder {
                    pane,
                    socket,
                    key,
                    epoch,
                    holder_pid,
                    child_pid,
                } => {
                    tx.execute(
                        "INSERT INTO holders (pane_id, socket, key, epoch, holder_pid, child_pid, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7)
                         ON CONFLICT(pane_id) DO UPDATE SET socket=?2, key=?3, epoch=?4, holder_pid=?5, child_pid=?6, updated_at=?7",
                        params![pane, socket, key, epoch as i64, holder_pid, child_pid, now],
                    )?;
                }
                Write::HolderDelete { pane } => {
                    tx.execute("DELETE FROM holders WHERE pane_id=?1", [pane.as_str()])?;
                    tx.execute("DELETE FROM vt_snapshots WHERE pane_id=?1", [pane.as_str()])?;
                }
                Write::Snapshot {
                    pane,
                    offset,
                    engine,
                    version,
                    blob,
                    incarnation,
                } => {
                    let blob = zstd::encode_all(&blob[..], 3)?;
                    tx.execute(
                        "INSERT INTO vt_snapshots (pane_id, holder_offset, engine, engine_version, blob, taken_at, holder_incarnation) VALUES (?1,?2,?3,?4,?5,?6,?7)
                         ON CONFLICT(pane_id) DO UPDATE SET holder_offset=?2, engine=?3, engine_version=?4, blob=?5, taken_at=?6, holder_incarnation=?7",
                        params![pane, offset as i64, engine, version, blob, now, incarnation],
                    )?;
                }
                Write::SnapshotDelete { pane } => {
                    tx.execute("DELETE FROM vt_snapshots WHERE pane_id=?1", [pane.as_str()])?;
                }
                Write::Kv { scope, key, value } => match value {
                    Some(v) => {
                        tx.execute(
                            "INSERT INTO kv (scope, key, value) VALUES (?1,?2,?3) ON CONFLICT(scope, key) DO UPDATE SET value=?3",
                            params![scope, key, v],
                        )?;
                    }
                    None => {
                        tx.execute(
                            "DELETE FROM kv WHERE scope=?1 AND key=?2",
                            params![scope, key],
                        )?;
                    }
                },
                Write::PolicyRule { id, rule } => match rule {
                    Some((json, by)) => {
                        tx.execute(
                            "INSERT INTO policy_rules (id, ord, rule_json, created_at, created_by)
                             VALUES (?1, (SELECT COALESCE(MAX(ord), 0) + 1 FROM policy_rules), ?2, ?3, ?4)
                             ON CONFLICT(id) DO UPDATE SET rule_json=?2",
                            params![id, json, now, by],
                        )?;
                    }
                    None => {
                        tx.execute("DELETE FROM policy_rules WHERE id=?1", [id.as_str()])?;
                    }
                },
                Write::Read { user, pane, rev } => {
                    tx.execute(
                        "INSERT INTO pane_reads (user, pane_id, seen_rev, seen_at) VALUES (?1,?2,?3,?4)
                         ON CONFLICT(user, pane_id) DO UPDATE SET seen_rev=?3, seen_at=?4",
                        params![user, pane, rev as i64, now],
                    )?;
                }
            }
        }
        let mut out = Vec::with_capacity(m.events.len());
        for e in m.events {
            tx.execute(
                "INSERT INTO events (ts, type, tier, subject_json, actor_json, data_json, v) VALUES (?1,?2,?3,?4,?5,?6,1)",
                params![now, e.kind, e.tier, e.subject.to_string(), e.actor.to_string(), e.data.to_string()],
            )?;
            let seq = tx.last_insert_rowid();
            out.push(Event {
                seq,
                ts: now,
                v: 1,
                tier: e.tier.into(),
                kind: e.kind,
                subject: e.subject,
                actor: e.actor,
                data: e.data,
            });
        }
        tx.commit()?;
        Ok(out)
    }

    pub fn load<T: DeserializeOwned>(&self, kind: &str) -> Result<Vec<T>> {
        let mut st = self
            .conn
            .prepare("SELECT json FROM entities WHERE kind=?1 AND closed=0 ORDER BY rowid")?;
        let rows = st.query_map([kind], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn get<T: DeserializeOwned>(&self, kind: &str, id: &str) -> Result<Option<T>> {
        let j: Option<String> = self
            .conn
            .query_row(
                "SELECT json FROM entities WHERE kind=?1 AND id=?2",
                params![kind, id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match j {
            Some(j) => Some(serde_json::from_str(&j)?),
            None => None,
        })
    }

    /// Any entity (open or closed) by id or handle.
    pub fn find<T: DeserializeOwned>(&self, kind: &str, id_or_handle: &str) -> Result<Option<T>> {
        let j: Option<String> = self
            .conn
            .query_row(
                "SELECT json FROM entities WHERE kind=?1 AND (id=?2 OR handle=?2) ORDER BY updated_at DESC LIMIT 1",
                params![kind, id_or_handle],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match j {
            Some(j) => Some(serde_json::from_str(&j)?),
            None => None,
        })
    }

    /// Every entity of `kind` (open and closed) whose JSON `task` field equals `task`, in
    /// insertion order. Uses the `entities_task` expression index.
    pub fn load_by_task<T: DeserializeOwned>(&self, kind: &str, task: &str) -> Result<Vec<T>> {
        let mut st = self.conn.prepare(
            "SELECT json FROM entities WHERE kind=?1 AND json_extract(json, '$.task')=?2 ORDER BY rowid",
        )?;
        let rows = st.query_map(params![kind, task], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// Every entity of `kind` (open and closed) whose JSON value at `path` (e.g.
    /// `$.acceptance.task_id`) equals `value`, in insertion order. Filters inside SQLite, so
    /// only matching rows are decoded; not indexed beyond `kind`.
    pub fn load_by_field<T: DeserializeOwned>(
        &self,
        kind: &str,
        path: &str,
        value: &str,
    ) -> Result<Vec<T>> {
        let mut st = self.conn.prepare(
            "SELECT json FROM entities WHERE kind=?1 AND json_extract(json, ?2)=?3 ORDER BY rowid",
        )?;
        let rows = st.query_map(params![kind, path, value], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// Closed (ended) entities of a kind, newest first.
    pub fn load_closed<T: DeserializeOwned>(&self, kind: &str, limit: usize) -> Result<Vec<T>> {
        let mut st = self.conn.prepare("SELECT json FROM entities WHERE kind=?1 AND closed=1 ORDER BY updated_at DESC LIMIT ?2")?;
        let rows = st.query_map(params![kind, limit as i64], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn holders(&self) -> Result<Vec<HolderRecord>> {
        let mut st = self
            .conn
            .prepare("SELECT pane_id, socket, key, epoch, holder_pid, child_pid FROM holders")?;
        let rows = st.query_map([], |r| {
            Ok(HolderRecord {
                pane: r.get(0)?,
                socket: r.get(1)?,
                key: r.get(2)?,
                epoch: r.get::<_, i64>(3)? as u64,
                holder_pid: r.get(4)?,
                child_pid: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn snapshot_for(&self, pane: &str) -> Result<Option<SnapshotRecord>> {
        let r = self
            .conn
            .query_row("SELECT holder_offset, engine, engine_version, blob, holder_incarnation FROM vt_snapshots WHERE pane_id=?1", [pane], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Vec<u8>>(3)?, r.get::<_, Option<String>>(4)?))
            })
            .optional()?;
        Ok(match r {
            Some((offset, engine, version, blob, incarnation)) => Some(SnapshotRecord {
                offset,
                engine,
                version,
                blob: zstd::decode_all(&blob[..])?,
                incarnation,
            }),
            None => None,
        })
    }

    pub fn kv_get(&self, scope: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM kv WHERE scope=?1 AND key=?2",
                params![scope, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Every `(key, value)` of a kv scope, ordered by key.
    pub fn kv_scope(&self, scope: &str) -> Result<Vec<(String, String)>> {
        let mut st = self
            .conn
            .prepare("SELECT key, value FROM kv WHERE scope=?1 ORDER BY key")?;
        let rows = st.query_map([scope], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Database file size in bytes (`page_count * page_size`).
    pub fn db_bytes(&self) -> Result<u64> {
        let pages: i64 = self.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let size: i64 = self.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok((pages.max(0) * size.max(0)) as u64)
    }

    /// API-added policy rules in order (`policy.list`, 07 §2.9).
    pub fn policy_rules(&self) -> Result<Vec<PolicyRuleRow>> {
        let mut st = self.conn.prepare(
            "SELECT id, ord, rule_json, created_at, created_by FROM policy_rules ORDER BY ord, id",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .map(|(id, ord, json, created_at, created_by)| PolicyRuleRow {
                id,
                ord,
                rule: serde_json::from_str(&json).unwrap_or(Value::Null),
                created_at,
                created_by,
            })
            .collect())
    }

    pub fn reads(&self, user: &str) -> Result<Vec<(String, u64)>> {
        let mut st = self
            .conn
            .prepare("SELECT pane_id, seen_rev FROM pane_reads WHERE user=?1")?;
        let rows = st.query_map([user], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn last_seq(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?)
    }

    pub fn earliest_seq(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MIN(seq), 0) FROM events", [], |r| r.get(0))?)
    }

    pub fn cursor(&self, seq: i64) -> Cursor {
        Cursor {
            machine_uuid: self.machine_uuid.clone(),
            session_uuid: self.session_uuid.clone(),
            log_epoch: self.log_epoch.clone(),
            seq,
        }
    }

    /// Events with seq > `after`, oldest first, optionally filtered by type globs.
    pub fn events_after(&self, after: i64, limit: usize, types: &[String]) -> Result<Vec<Event>> {
        let mut st = self.conn.prepare(
            "SELECT seq, ts, v, tier, type, subject_json, actor_json, data_json FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = st.query_map(params![after, limit as i64 * 4], |r| {
            Ok(Event {
                seq: r.get(0)?,
                ts: r.get(1)?,
                v: r.get::<_, i64>(2)? as u32,
                tier: r.get(3)?,
                kind: r.get(4)?,
                subject: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(Value::Null),
                actor: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or(Value::Null),
                data: serde_json::from_str(&r.get::<_, String>(7)?).unwrap_or(Value::Null),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            let e = r?;
            if types.is_empty() || types.iter().any(|g| glob_match(g, &e.kind)) {
                out.push(e);
                if out.len() >= limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Retention (02 §2.3): prune `sync` events older than `sync_days`, `history` older than `history_days`.
    pub fn prune(&self, sync_days: i64, history_days: i64) -> Result<usize> {
        let r = self.prune_with(&Retention {
            sync_days,
            history_days,
            max_rows: 0,
        })?;
        Ok(r.aged)
    }

    /// Retention with the row cap (02 §2.3): events past their tier's age go first, then, if the
    /// log is still over `max_rows` (0 = no cap), the oldest `sync` rows, and only then the oldest
    /// `history` rows. The newest row is never removed, so `seq` can't restart below its past.
    pub fn prune_with(&self, r: &Retention) -> Result<PruneReport> {
        let now = now_ms();
        let aged = self.conn.execute(
            "DELETE FROM events WHERE ((tier='sync' AND ts < ?1) OR (tier='history' AND ts < ?2))
               AND seq < (SELECT MAX(seq) FROM events)",
            params![
                now - r.sync_days.saturating_mul(86_400_000),
                now - r.history_days.saturating_mul(86_400_000)
            ],
        )?;
        let mut capped = 0;
        if r.max_rows > 0 {
            for tier in ["sync", "history"] {
                let count: i64 = self
                    .conn
                    .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
                let excess = count - r.max_rows;
                if excess <= 0 {
                    break;
                }
                capped += self.conn.execute(
                    "DELETE FROM events WHERE seq IN (
                       SELECT seq FROM events WHERE tier=?1 AND seq < (SELECT MAX(seq) FROM events)
                       ORDER BY seq LIMIT ?2)",
                    params![tier, excess],
                )?;
            }
        }
        Ok(PruneReport { aged, capped })
    }

    /// Number of rows in the event log.
    pub fn event_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?)
    }

    /// Make every write fail with SQLite's read-only error (`PRAGMA query_only`), as a full or
    /// failing disk would. A diagnostic and test hook for degraded mode (02 §4a); the server
    /// never calls it.
    pub fn set_query_only(&self, on: bool) -> Result<()> {
        self.conn.pragma_update(None, "query_only", on)?;
        Ok(())
    }

    /// Cheap write probe used to leave degraded mode (02 §4a).
    pub fn probe(&self) -> Result<()> {
        self.conn.execute("INSERT INTO meta (key, value) VALUES ('probe', ?1) ON CONFLICT(key) DO UPDATE SET value=?1", [now_ms().to_string()])?;
        Ok(())
    }

    pub fn fts_insert(&self, rows: &[(String, u64, i64, String)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO scrollback_fts (pane_id, line_no, ts, text) VALUES (?1,?2,?3,?4)",
            )?;
            for (p, l, ts, t) in rows {
                st.execute(params![p, *l as i64, ts, t])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Remember which workspace/tab an archived pane belongs to (upsert).
    pub fn fts_register_panes(&self, panes: &[ArchivePane]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO archive_panes (pane_id, workspace, tab, handle, title, updated_at) VALUES (?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(pane_id) DO UPDATE SET workspace=?2, tab=?3, handle=?4, title=?5, updated_at=?6",
            )?;
            for (p, w, t, h, ti) in panes {
                st.execute(params![p, w, t, h, ti, now_ms()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The recorded identity of an archived pane (it may have closed).
    pub fn archive_pane(&self, pane: &str) -> Result<Option<ArchivePane>> {
        Ok(self
            .conn
            .query_row(
                "SELECT pane_id, workspace, tab, handle, title FROM archive_panes WHERE pane_id=?1 OR handle=?1 ORDER BY updated_at DESC LIMIT 1",
                [pane],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    ))
                },
            )
            .optional()?)
    }

    /// Archive search with pane/workspace/time filters, newest first.
    pub fn fts_query(&self, f: &FtsQuery) -> Result<Vec<FtsHit>> {
        use rusqlite::types::Value as V;
        let query = fts_quote(&f.q);
        if query.is_empty() {
            return Ok(vec![]);
        }
        let mut sql = String::from(
            "SELECT scrollback_fts.pane_id, scrollback_fts.line_no, scrollback_fts.ts, scrollback_fts.text,
                    a.workspace, a.handle, a.title
             FROM scrollback_fts LEFT JOIN archive_panes a ON a.pane_id = scrollback_fts.pane_id
             WHERE scrollback_fts MATCH ?",
        );
        let mut args: Vec<V> = vec![V::Text(query)];
        let list = |sql: &mut String, col: &str, items: &[String], args: &mut Vec<V>| {
            if items.is_empty() {
                sql.push_str(" AND 0");
                return;
            }
            sql.push_str(&format!(" AND {col} IN ("));
            for (i, it) in items.iter().enumerate() {
                sql.push_str(if i == 0 { "?" } else { ",?" });
                args.push(V::Text(it.clone()));
            }
            sql.push(')');
        };
        if let Some(p) = &f.panes {
            list(&mut sql, "scrollback_fts.pane_id", p, &mut args);
        }
        if let Some(w) = &f.workspaces {
            list(&mut sql, "a.workspace", w, &mut args);
        }
        if let Some(since) = f.since_ms {
            sql.push_str(" AND scrollback_fts.ts >= ?");
            args.push(V::Integer(since));
        }
        sql.push_str(" ORDER BY scrollback_fts.rowid DESC LIMIT ?");
        args.push(V::Integer(f.limit.max(1) as i64));
        let mut st = self.conn.prepare(&sql)?;
        let rows = st
            .query_map(rusqlite::params_from_iter(args), |r| {
                Ok(FtsHit {
                    pane: r.get(0)?,
                    line: r.get::<_, i64>(1)? as u64,
                    ts: r.get(2)?,
                    text: r.get(3)?,
                    workspace: r.get(4)?,
                    handle: r.get(5)?,
                    title: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Full-text search over archived scrollback. Returns (pane, line, ts, text).
    pub fn fts_search(
        &self,
        q: &str,
        pane: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, u64, i64, String)>> {
        let query = fts_quote(q);
        let sql = if pane.is_some() {
            "SELECT pane_id, line_no, ts, text FROM scrollback_fts WHERE scrollback_fts MATCH ?1 AND pane_id = ?3 ORDER BY rowid DESC LIMIT ?2"
        } else {
            "SELECT pane_id, line_no, ts, text FROM scrollback_fts WHERE scrollback_fts MATCH ?1 ORDER BY rowid DESC LIMIT ?2"
        };
        let mut st = self.conn.prepare(sql)?;
        let map = |r: &rusqlite::Row| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        };
        let rows: Vec<_> = match pane {
            Some(p) => st
                .query_map(params![query, limit as i64, p], map)?
                .collect::<Result<_, _>>()?,
            None => st
                .query_map(params![query, limit as i64], map)?
                .collect::<Result<_, _>>()?,
        };
        Ok(rows)
    }
}

/// Words become quoted FTS5 tokens (AND); a trailing `*` keeps prefix matching (`migr*`).
fn fts_quote(q: &str) -> String {
    q.split_whitespace()
        .filter_map(|w| {
            let (w, prefix) = match w.strip_suffix('*') {
                Some(x) => (x, true),
                None => (w, false),
            };
            (!w.is_empty()).then(|| {
                format!(
                    "\"{}\"{}",
                    w.replace('"', "\"\""),
                    if prefix { "*" } else { "" }
                )
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Make an existing regular file of ours 0600 (no-op when absent, foreign or a symlink).
pub fn restrict_file(path: &Path) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(m) = std::fs::symlink_metadata(path) else {
        return;
    };
    // SAFETY: getuid has no preconditions.
    if !m.is_file() || m.uid() != unsafe { libc::getuid() } || m.mode() & 0o077 == 0 {
        return;
    }
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// A read-only view of a session database for diagnostics (`vibeke debug bundle`, 09 §9.5):
/// opened without migrating, so it is safe while a server runs. Exposes the schema and table
/// sizes only, never row content.
pub struct Diagnostics {
    conn: Connection,
}

impl Diagnostics {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open {} read-only", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        Ok(Diagnostics { conn })
    }

    /// Table names with their row counts.
    pub fn table_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut st = self.conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let names = st
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::new();
        for n in names {
            let q = format!("SELECT COUNT(*) FROM \"{}\"", n.replace('"', "\"\""));
            let c: i64 = self.conn.query_row(&q, [], |r| r.get(0)).unwrap_or(-1);
            out.push((n, c));
        }
        Ok(out)
    }

    /// `CREATE` statements of the schema (no data).
    pub fn schema_sql(&self) -> Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type, name")?;
        let v = st
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    /// The applied schema migration version.
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
            [],
            |r| r.get(0),
        )?)
    }

    /// The pane token hashes (blake3 hex) — never tokens — so a bundle can recognise and
    /// redact a token that a pane printed.
    pub fn token_hashes(&self) -> Vec<String> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM kv WHERE scope='server' AND key='pane_token_hashes'",
                [],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        v.and_then(|s| serde_json::from_str::<serde_json::Map<String, Value>>(&s).ok())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }
}

pub fn glob_match(glob: &str, s: &str) -> bool {
    if let Some(p) = glob.strip_suffix('*') {
        return s.starts_with(p);
    }
    if let (Some(a), Some(b)) = (glob.find('{'), glob.find('}')) {
        let (pre, alts, post) = (&glob[..a], &glob[a + 1..b], &glob[b + 1..]);
        return alts.split(',').any(|alt| s == format!("{pre}{alt}{post}"));
    }
    glob == s
}

/// Machine identity, generated once per machine install (02 §2.3), stored next to the
/// session directories.
fn machine_uuid(dir: Option<&Path>) -> Result<String> {
    let Some(dir) = dir.and_then(|d| d.parent()) else {
        return Ok(ulid::Ulid::new().to_string());
    };
    let p = dir.join("machine-uuid");
    if let Ok(s) = std::fs::read_to_string(&p) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }
    let id = ulid::Ulid::new().to_string();
    std::fs::create_dir_all(dir)?;
    std::fs::write(&p, &id)?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn outbox_is_transactional_and_ordered() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::open(&d.path().join("s/state.db")).unwrap();
        let mut m = Mutation::new();
        m.put("pane", "p1", Some("w1:p1"), &json!({"id":"p1"}));
        m.event("pane.created", json!({"pane":"p1"}), json!({}));
        let ev = s.commit(m).unwrap();
        assert_eq!(ev[0].seq, 1);
        let mut m = Mutation::new();
        m.put("pane", "p2", Some("w1:p2"), &json!({"id":"p2"}));
        m.event("pane.created", json!({"pane":"p2"}), json!({}));
        m.event("interaction.opened", json!({}), json!({}));
        let ev = s.commit(m).unwrap();
        assert_eq!(ev.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(ev[1].tier, "history");
        let panes: Vec<Value> = s.load("pane").unwrap();
        assert_eq!(panes.len(), 2);
        assert_eq!(s.events_after(1, 10, &[]).unwrap().len(), 2);
        assert_eq!(
            s.events_after(0, 10, &["interaction.*".into()])
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            s.events_after(0, 10, &["pane.{created,closed}".into()])
                .unwrap()
                .len(),
            2
        );
        let uuid = s.session_uuid.clone();
        drop(s);
        let s = Store::open(&d.path().join("s/state.db")).unwrap();
        assert_eq!(s.session_uuid, uuid);
        assert_eq!(s.last_seq().unwrap(), 3);
    }

    #[test]
    fn per_task_and_field_lookups_cover_open_and_closed_history() {
        let mut s = Store::open_in_memory().unwrap();
        let mut m = Mutation::new();
        // Lots of unrelated closed history must not hide a task's older records.
        m.close("check_run", "old-a", None, &json!({"task": "A", "n": 0}));
        for i in 0..50 {
            m.close("check_run", &format!("b{i}"), None, &json!({"task": "B"}));
        }
        m.put("check_run", "new-a", None, &json!({"task": "A", "n": 1}));
        m.put("acc", "x", None, &json!({"acceptance": {"task_id": "A"}}));
        m.put("acc", "y", None, &json!({"acceptance": {"task_id": "B"}}));
        s.commit(m).unwrap();
        let a: Vec<Value> = s.load_by_task("check_run", "A").unwrap();
        assert_eq!(
            a.iter().map(|v| v["n"].clone()).collect::<Vec<_>>(),
            vec![json!(0), json!(1)]
        );
        let acc: Vec<Value> = s.load_by_field("acc", "$.acceptance.task_id", "A").unwrap();
        assert_eq!(acc.len(), 1);
        let plan: String = s
            .conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT json FROM entities WHERE kind='check_run' AND json_extract(json, '$.task')='A'",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plan.contains("entities_task"), "{plan}");
    }

    #[test]
    fn snapshots_and_holders() {
        let mut s = Store::open_in_memory().unwrap();
        let mut m = Mutation::new();
        m.holder("p1", "/tmp/x.sock", &[1, 2, 3], 4, Some(10), Some(11));
        m.snapshot("p1", 99, "e", "1", vec![7; 1000], "11:123");
        s.commit(m).unwrap();
        let h = s.holders().unwrap();
        assert_eq!(h[0].epoch, 4);
        let snap = s.snapshot_for("p1").unwrap().unwrap();
        assert_eq!(snap.offset, 99);
        assert_eq!(snap.blob, vec![7; 1000]);
        assert_eq!(snap.incarnation.as_deref(), Some("11:123"));
        let mut m = Mutation::new();
        m.snapshot_delete("p1");
        s.commit(m).unwrap();
        assert!(s.snapshot_for("p1").unwrap().is_none());
    }

    #[test]
    fn fts() {
        let s = Store::open_in_memory().unwrap();
        s.fts_insert(&[
            ("p1".into(), 1, 0, "migration failed: relation users".into()),
            ("p2".into(), 2, 0, "all good".into()),
        ])
        .unwrap();
        let r = s.fts_search("migration failed", None, 10).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, "p1");
    }

    /// `policy_rules` (migration 5): ordered appends, replace keeps the position, delete, and
    /// the write commits atomically with its event.
    #[test]
    fn policy_rules_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("state.db");
        let mut s = Store::open(&path).unwrap();
        let mut m = Mutation::new();
        m.policy_rule_put("p1", &serde_json::json!({"effect": "deny"}), Some("cli"))
            .policy_rule_put("p2", &serde_json::json!({"effect": "allow"}), None)
            .event("policy.rule_added", Value::Null, Value::Null);
        assert_eq!(s.commit(m).unwrap().len(), 1);
        let mut m = Mutation::new();
        m.policy_rule_put("p1", &serde_json::json!({"effect": "ask"}), None);
        s.commit(m).unwrap();
        let rows = s.policy_rules().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["p1", "p2"]
        );
        assert_eq!(rows[0].rule["effect"], "ask");
        assert_eq!(rows[0].created_by.as_deref(), Some("cli"));
        let mut m = Mutation::new();
        m.policy_rule_delete("p1");
        s.commit(m).unwrap();
        assert_eq!(s.policy_rules().unwrap().len(), 1);
        drop(s);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "state.db is 0600");
        let diag = Diagnostics::open(&path).unwrap();
        assert!(diag.schema_version().unwrap() >= 5);
        assert!(
            diag.table_counts()
                .unwrap()
                .iter()
                .any(|(n, c)| n == "policy_rules" && *c == 1)
        );
        assert!(
            diag.schema_sql()
                .unwrap()
                .iter()
                .any(|s| s.contains("policy_rules"))
        );
    }

    #[test]
    fn fts_query_filters() {
        let s = Store::open_in_memory().unwrap();
        s.fts_insert(&[
            (
                "p1".into(),
                1,
                100,
                "migration failed: relation users".into(),
            ),
            ("p2".into(), 7, 200, "migration ok".into()),
            ("p3".into(), 9, 300, "migrations pending".into()),
        ])
        .unwrap();
        s.fts_register_panes(&[
            (
                "p1".into(),
                "w1".into(),
                "t1".into(),
                "w1:p1".into(),
                "a".into(),
            ),
            (
                "p2".into(),
                "w2".into(),
                "t2".into(),
                "w2:p2".into(),
                "b".into(),
            ),
        ])
        .unwrap();
        let q = |f: FtsQuery| s.fts_query(&f).unwrap();
        let base = FtsQuery {
            q: "migration".into(),
            limit: 10,
            ..Default::default()
        };
        assert_eq!(q(base.clone()).len(), 2);
        let w1 = q(FtsQuery {
            workspaces: Some(vec!["w1".into()]),
            ..base.clone()
        });
        assert_eq!(w1.len(), 1);
        assert_eq!(w1[0].handle.as_deref(), Some("w1:p1"));
        assert_eq!(
            q(FtsQuery {
                since_ms: Some(150),
                ..base.clone()
            })[0]
                .pane,
            "p2"
        );
        assert_eq!(
            q(FtsQuery {
                q: "migr*".into(),
                ..base.clone()
            })
            .len(),
            3
        );
        assert!(
            q(FtsQuery {
                panes: Some(vec![]),
                ..base
            })
            .is_empty()
        );
        assert_eq!(s.archive_pane("w2:p2").unwrap().unwrap().1, "w2");
    }
}

#[cfg(test)]
mod retention_tests;
