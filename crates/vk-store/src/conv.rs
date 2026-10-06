//! Conversation index for the session desk (research R2): native harness transcripts split into
//! (session, turn, role/kind, text, timestamp, source offset) rows in an FTS5 table.
//!
//! It is a **separate SQLite file** from the session state database (`desk.db` next to
//! `state.db`): it is derived, rebuildable content that is written by a background indexer, so
//! it never holds the state lock and never shares migrations with the state tables. Scrollback
//! search (`scrollback_fts`) is unrelated and unchanged.
//!
//! Sources are tracked incrementally by (size, mtime, byte offset of the last complete line,
//! turns seen so far); a file that shrinks is re-indexed from the start. Forgotten sessions are
//! tombstoned so the indexer does not resurrect them.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS conv_sources (
    path TEXT PRIMARY KEY, machine TEXT NOT NULL, harness TEXT NOT NULL, session TEXT,
    cwd TEXT, repo TEXT, workspace TEXT, run TEXT, origin TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0, mtime_ms INTEGER NOT NULL DEFAULT 0,
    offset INTEGER NOT NULL DEFAULT 0, turns INTEGER NOT NULL DEFAULT 0,
    first_ts INTEGER, last_ts INTEGER, rows INTEGER NOT NULL DEFAULT 0, indexed_at INTEGER);
CREATE TABLE IF NOT EXISTS conv_forgotten (session TEXT PRIMARY KEY, at_ms INTEGER NOT NULL);
CREATE VIRTUAL TABLE IF NOT EXISTS conv_fts USING fts5(
    text, session UNINDEXED, path UNINDEXED, harness UNINDEXED, machine UNINDEXED,
    repo UNINDEXED, cwd UNINDEXED, turn UNINDEXED, role UNINDEXED, kind UNINDEXED,
    ts UNINDEXED, offset UNINDEXED);
"#;

/// One transcript file the indexer knows about.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConvSource {
    pub path: String,
    pub machine: String,
    pub harness: String,
    /// Native session id (from the run, or discovered in the transcript).
    pub session: Option<String>,
    pub cwd: Option<String>,
    /// Main repository root of `cwd` (worktrees of one repository group together).
    pub repo: Option<String>,
    /// Workspace of the pane the run was in (pane-scoped authorization); `None` for files found
    /// only under an opted-in transcript root.
    pub workspace: Option<String>,
    pub run: Option<String>,
    /// `run` (Vibeke saw a run with this transcript) or `root` (opted-in transcript root).
    pub origin: String,
    pub size: u64,
    pub mtime_ms: i64,
    /// Byte offset just past the last complete line indexed.
    pub offset: u64,
    /// Turns seen so far (the next user prompt starts turn `turns + 1`).
    pub turns: u32,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub rows: u64,
    pub indexed_at: Option<i64>,
}

/// One indexed transcript item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConvRow {
    pub turn: u32,
    /// `user` | `assistant` | `tool`.
    pub role: String,
    /// `text` | `tool_call` | `tool_result`.
    pub kind: String,
    pub text: String,
    pub ts: i64,
    /// Byte offset of the transcript line this item came from.
    pub offset: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ConvQuery {
    pub text: String,
    /// Main repository root or a directory prefix of the session cwd.
    pub repo: Option<String>,
    pub harness: Option<String>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub session: Option<String>,
    /// Only sessions whose source belongs to this workspace (pane-scoped callers).
    pub workspace: Option<String>,
    /// `relevance` (default) or `recent`.
    pub recent: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConvHit {
    pub session: String,
    pub path: String,
    pub harness: String,
    pub machine: String,
    pub repo: Option<String>,
    pub cwd: Option<String>,
    pub turn: u32,
    pub role: String,
    pub kind: String,
    pub ts: i64,
    pub offset: u64,
    pub snippet: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConvSession {
    pub session: String,
    pub harness: String,
    pub machine: String,
    pub repo: Option<String>,
    pub cwd: Option<String>,
    pub workspace: Option<String>,
    pub run: Option<String>,
    pub first_ts: Option<i64>,
    pub last_ts: Option<i64>,
    pub turns: u32,
    pub rows: u64,
    pub paths: Vec<String>,
}

pub struct ConvIndex {
    conn: Connection,
}

fn fts_quote(q: &str) -> String {
    q.split_whitespace()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn source_of(r: &rusqlite::Row) -> rusqlite::Result<ConvSource> {
    Ok(ConvSource {
        path: r.get(0)?,
        machine: r.get(1)?,
        harness: r.get(2)?,
        session: r.get(3)?,
        cwd: r.get(4)?,
        repo: r.get(5)?,
        workspace: r.get(6)?,
        run: r.get(7)?,
        origin: r.get(8)?,
        size: r.get::<_, i64>(9)? as u64,
        mtime_ms: r.get(10)?,
        offset: r.get::<_, i64>(11)? as u64,
        turns: r.get::<_, i64>(12)? as u32,
        first_ts: r.get(13)?,
        last_ts: r.get(14)?,
        rows: r.get::<_, i64>(15)? as u64,
        indexed_at: r.get(16)?,
    })
}

const SOURCE_COLS: &str = "path, machine, harness, session, cwd, repo, workspace, run, origin, size, mtime_ms, offset, turns, first_ts, last_ts, rows, indexed_at";

impl ConvIndex {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let conn = Connection::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if v > SCHEMA_VERSION {
            // A newer Vibeke wrote this derived index: start over rather than misread it.
            conn.execute_batch(
                "DROP TABLE IF EXISTS conv_sources; DROP TABLE IF EXISTS conv_forgotten; DROP TABLE IF EXISTS conv_fts;",
            )?;
        }
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(ConvIndex { conn })
    }

    pub fn sources(&self) -> Result<Vec<ConvSource>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {SOURCE_COLS} FROM conv_sources ORDER BY path"
        ))?;
        let rows = st.query_map([], source_of)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn source(&self, path: &str) -> Result<Option<ConvSource>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {SOURCE_COLS} FROM conv_sources WHERE path=?1"),
                [path],
                source_of,
            )
            .optional()?)
    }

    fn put_source(conn: &Connection, s: &ConvSource) -> Result<()> {
        conn.execute(
            &format!(
                "INSERT OR REPLACE INTO conv_sources ({SOURCE_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)"
            ),
            params![
                s.path,
                s.machine,
                s.harness,
                s.session,
                s.cwd,
                s.repo,
                s.workspace,
                s.run,
                s.origin,
                s.size as i64,
                s.mtime_ms,
                s.offset as i64,
                s.turns as i64,
                s.first_ts,
                s.last_ts,
                s.rows as i64,
                s.indexed_at,
            ],
        )?;
        Ok(())
    }

    /// Insert or update a source's metadata (offsets are whatever `s` carries).
    pub fn put(&self, s: &ConvSource) -> Result<()> {
        Self::put_source(&self.conn, s)
    }

    /// Append rows for a source and store its advanced cursor, in one transaction.
    pub fn append(&mut self, s: &ConvSource, rows: &[ConvRow]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO conv_fts (text, session, path, harness, machine, repo, cwd, turn, role, kind, ts, offset) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            )?;
            let session = s.session.clone().unwrap_or_default();
            for r in rows {
                st.execute(params![
                    r.text,
                    session,
                    s.path,
                    s.harness,
                    s.machine,
                    s.repo,
                    s.cwd,
                    r.turn as i64,
                    r.role,
                    r.kind,
                    r.ts,
                    r.offset as i64,
                ])?;
            }
        }
        Self::put_source(&tx, s)?;
        tx.commit()?;
        Ok(())
    }

    /// Drop a source's rows and rewind its cursor (file truncated or replaced).
    pub fn reset(&mut self, path: &str) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let n = tx.execute("DELETE FROM conv_fts WHERE path=?1", [path])?;
        tx.execute(
            "UPDATE conv_sources SET offset=0, turns=0, rows=0, size=0, mtime_ms=0, first_ts=NULL, last_ts=NULL WHERE path=?1",
            [path],
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// Remove a source and its rows entirely (excluded by configuration).
    pub fn remove_source(&mut self, path: &str) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let n = tx.execute("DELETE FROM conv_fts WHERE path=?1", [path])?;
        tx.execute("DELETE FROM conv_sources WHERE path=?1", [path])?;
        tx.commit()?;
        Ok(n)
    }

    pub fn search(&self, q: &ConvQuery) -> Result<Vec<ConvHit>> {
        let query = fts_quote(&q.text);
        if query.is_empty() {
            return Ok(vec![]);
        }
        let mut sql = String::from(
            "SELECT f.session, f.path, f.harness, f.machine, f.repo, f.cwd, f.turn, f.role, f.kind, f.ts, f.offset, snippet(conv_fts, 0, '«', '»', '…', 16) FROM conv_fts f WHERE conv_fts MATCH ?1",
        );
        let mut args: Vec<rusqlite::types::Value> = vec![query.into()];
        let mut push = |sql: &mut String, clause: &str, v: rusqlite::types::Value| {
            args.push(v);
            sql.push_str(&clause.replace('?', &format!("?{}", args.len())));
        };
        if let Some(r) = &q.repo {
            let like = format!("{}%", r.trim_end_matches('/'));
            push(&mut sql, " AND (f.repo = ? ", r.clone().into());
            push(&mut sql, "OR f.cwd LIKE ?)", like.into());
        }
        if let Some(h) = &q.harness {
            push(&mut sql, " AND f.harness = ?", h.clone().into());
        }
        if let Some(t) = q.since_ms {
            push(&mut sql, " AND f.ts >= ?", t.into());
        }
        if let Some(t) = q.until_ms {
            push(&mut sql, " AND f.ts < ?", t.into());
        }
        if let Some(sess) = &q.session {
            push(&mut sql, " AND f.session = ?", sess.clone().into());
        }
        if let Some(ws) = &q.workspace {
            push(
                &mut sql,
                " AND f.path IN (SELECT path FROM conv_sources WHERE workspace = ?)",
                ws.clone().into(),
            );
        }
        sql.push_str(if q.recent {
            " ORDER BY f.ts DESC, f.rowid DESC"
        } else {
            " ORDER BY rank"
        });
        args.push((q.limit.clamp(1, 500) as i64).into());
        sql.push_str(&format!(" LIMIT ?{}", args.len()));
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(args), |r| {
            Ok(ConvHit {
                session: r.get(0)?,
                path: r.get(1)?,
                harness: r.get(2)?,
                machine: r.get(3)?,
                repo: r.get(4)?,
                cwd: r.get(5)?,
                turn: r.get::<_, i64>(6)? as u32,
                role: r.get(7)?,
                kind: r.get(8)?,
                ts: r.get(9)?,
                offset: r.get::<_, i64>(10)? as u64,
                snippet: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Sessions (grouped sources), most recently active first.
    pub fn sessions(&self) -> Result<Vec<ConvSession>> {
        let mut by: BTreeMap<String, ConvSession> = BTreeMap::new();
        let mut srcs = self.sources()?;
        // Newest last so its metadata wins.
        srcs.sort_by_key(|s| s.last_ts.unwrap_or(0));
        for s in srcs {
            let Some(id) = s.session.clone() else {
                continue;
            };
            let e = by.entry(id.clone()).or_insert_with(|| ConvSession {
                session: id,
                harness: s.harness.clone(),
                machine: s.machine.clone(),
                repo: None,
                cwd: None,
                workspace: None,
                run: None,
                first_ts: None,
                last_ts: None,
                turns: 0,
                rows: 0,
                paths: vec![],
            });
            e.harness = s.harness.clone();
            e.repo = s.repo.clone().or(e.repo.take());
            e.cwd = s.cwd.clone().or(e.cwd.take());
            e.workspace = s.workspace.clone().or(e.workspace.take());
            e.run = s.run.clone().or(e.run.take());
            e.first_ts = match (e.first_ts, s.first_ts) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            e.last_ts = e.last_ts.max(s.last_ts);
            e.turns = e.turns.max(s.turns);
            e.rows += s.rows;
            e.paths.push(s.path.clone());
        }
        let mut v: Vec<ConvSession> = by.into_values().collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.last_ts));
        Ok(v)
    }

    /// Indexed items of one session, optionally limited to a turn range `[from, to]` and to
    /// one source path, in transcript order.
    pub fn turns(
        &self,
        session: &str,
        path: Option<&str>,
        from: u32,
        to: u32,
    ) -> Result<Vec<(String, ConvRow)>> {
        let mut st = self.conn.prepare(
            "SELECT path, turn, role, kind, text, ts, offset FROM conv_fts WHERE session=?1 AND turn>=?2 AND turn<=?3 AND (?4 IS NULL OR path=?4) ORDER BY path, offset, rowid",
        )?;
        let rows = st.query_map(params![session, from as i64, to as i64, path], |r| {
            Ok((
                r.get::<_, String>(0)?,
                ConvRow {
                    turn: r.get::<_, i64>(1)? as u32,
                    role: r.get(2)?,
                    kind: r.get(3)?,
                    text: r.get(4)?,
                    ts: r.get(5)?,
                    offset: r.get::<_, i64>(6)? as u64,
                },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Delete a session's rows; with `tombstone`, remember it so it is never re-indexed.
    /// Source cursors stay where they are, so already-read bytes are not read again.
    pub fn forget_session(&mut self, session: &str, tombstone: bool, now_ms: i64) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let n = tx.execute("DELETE FROM conv_fts WHERE session=?1", [session])?;
        tx.execute("UPDATE conv_sources SET rows=0 WHERE session=?1", [session])?;
        if tombstone {
            tx.execute(
                "INSERT OR REPLACE INTO conv_forgotten (session, at_ms) VALUES (?1, ?2)",
                params![session, now_ms],
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Sessions of a repository (root or cwd prefix).
    pub fn sessions_in_repo(&self, repo: &str) -> Result<Vec<String>> {
        let like = format!("{}%", repo.trim_end_matches('/'));
        let mut st = self.conn.prepare(
            "SELECT DISTINCT session FROM conv_sources WHERE session IS NOT NULL AND (repo=?1 OR cwd LIKE ?2)",
        )?;
        let rows = st.query_map(params![repo, like], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn sessions_in_workspace(&self, ws: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare(
            "SELECT DISTINCT session FROM conv_sources WHERE session IS NOT NULL AND workspace=?1",
        )?;
        let rows = st.query_map([ws], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn forgotten(&self) -> Result<HashSet<String>> {
        let mut st = self.conn.prepare("SELECT session FROM conv_forgotten")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Retention / `forget --before`: drop rows older than `before_ms`.
    pub fn prune(&mut self, before_ms: i64) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let n = tx.execute("DELETE FROM conv_fts WHERE ts < ?1", [before_ms])?;
        if n > 0 {
            tx.execute(
                "UPDATE conv_sources SET rows=(SELECT COUNT(*) FROM conv_fts WHERE conv_fts.path=conv_sources.path)",
                [],
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// (sources, indexed rows, tombstoned sessions).
    pub fn stats(&self) -> Result<(u64, u64, u64)> {
        let one = |sql: &str| -> Result<u64> {
            Ok(self.conn.query_row(sql, [], |r| r.get::<_, i64>(0))? as u64)
        };
        Ok((
            one("SELECT COUNT(*) FROM conv_sources")?,
            one("SELECT COUNT(*) FROM conv_fts")?,
            one("SELECT COUNT(*) FROM conv_forgotten")?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(path: &str, session: &str, harness: &str, repo: &str) -> ConvSource {
        ConvSource {
            path: path.into(),
            machine: "m".into(),
            harness: harness.into(),
            session: Some(session.into()),
            cwd: Some(format!("{repo}/sub")),
            repo: Some(repo.into()),
            workspace: Some("ws1".into()),
            origin: "run".into(),
            ..Default::default()
        }
    }

    fn row(turn: u32, role: &str, text: &str, ts: i64) -> ConvRow {
        ConvRow {
            turn,
            role: role.into(),
            kind: "text".into(),
            text: text.into(),
            ts,
            offset: ts as u64,
        }
    }

    #[test]
    fn search_filters_forget_and_prune() {
        let mut ix = ConvIndex::open_in_memory().unwrap();
        let mut a = src("/t/a.jsonl", "s-a", "claude", "/r/one");
        a.offset = 10;
        a.turns = 2;
        a.last_ts = Some(200);
        ix.append(
            &a,
            &[
                row(1, "user", "fix the login redirect", 100),
                row(2, "assistant", "the redirect loop is fixed", 200),
            ],
        )
        .unwrap();
        let mut b = src("/t/b.jsonl", "s-b", "codex", "/r/two");
        b.workspace = Some("ws2".into());
        b.last_ts = Some(300);
        ix.append(&b, &[row(1, "user", "redirect for docs site", 300)])
            .unwrap();

        fn find(ix: &ConvIndex, f: &dyn Fn(&mut ConvQuery)) -> Vec<ConvHit> {
            let mut q = ConvQuery {
                text: "redirect".into(),
                limit: 50,
                ..Default::default()
            };
            f(&mut q);
            ix.search(&q).unwrap()
        }
        let q = |f: &dyn Fn(&mut ConvQuery)| find(&ix, f);
        assert_eq!(q(&|_| {}).len(), 3);
        assert_eq!(q(&|q| q.harness = Some("codex".into())).len(), 1);
        assert_eq!(q(&|q| q.repo = Some("/r/one".into())).len(), 2);
        assert_eq!(
            q(&|q| q.repo = Some("/r/one/sub".into())).len(),
            2,
            "cwd prefix"
        );
        assert_eq!(q(&|q| q.since_ms = Some(150)).len(), 2);
        assert_eq!(q(&|q| q.until_ms = Some(150)).len(), 1);
        assert_eq!(q(&|q| q.workspace = Some("ws2".into())).len(), 1);
        let recent = q(&|q| q.recent = true);
        assert_eq!(recent[0].session, "s-b");
        assert!(
            recent[0].snippet.contains("«redirect»"),
            "{}",
            recent[0].snippet
        );

        let sessions = ix.sessions().unwrap();
        assert_eq!(sessions[0].session, "s-b", "most recent first");
        assert_eq!(ix.turns("s-a", None, 2, 2).unwrap().len(), 1);

        assert_eq!(ix.forget_session("s-a", true, 1).unwrap(), 2);
        assert_eq!(find(&ix, &|_| {}).len(), 1);
        assert!(ix.forgotten().unwrap().contains("s-a"));
        assert_eq!(ix.prune(400).unwrap(), 1);
        assert_eq!(ix.stats().unwrap(), (2, 0, 1));
        // Cursors survive a forget: already-read bytes are not read again.
        assert_eq!(ix.source("/t/a.jsonl").unwrap().unwrap().offset, 10);
        ix.reset("/t/a.jsonl").unwrap();
        assert_eq!(ix.source("/t/a.jsonl").unwrap().unwrap().offset, 0);
    }
}
