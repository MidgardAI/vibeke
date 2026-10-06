//! Native plugin storage (07 §7.5): `plugin_kv`, a per-plugin namespace in `state.db` with a
//! per-value limit (1 MiB) and a per-plugin total quota (64 MiB by default, configurable by the
//! caller), and `plugin_commands`, the session's native plugin command records (02 §3: invocation
//! id, plugin, argv, context, start/end, status/exit, log reference).

use crate::{Store, now_ms};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Largest value one key may hold (07 §7.5).
pub const MAX_VALUE: usize = 1024 * 1024;
/// Default total quota per plugin (07 §7.5; `[plugins] kv_quota_bytes` overrides).
pub const DEFAULT_QUOTA: u64 = 64 * 1024 * 1024;
/// Longest key.
pub const MAX_KEY: usize = 512;

#[derive(Debug, PartialEq)]
pub enum KvError {
    ValueTooLarge(usize),
    Quota { quota: u64, used: u64, needed: u64 },
    Key(String),
}

impl std::fmt::Display for KvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KvError::ValueTooLarge(n) => write!(f, "value of {n} bytes exceeds the 1 MiB limit"),
            KvError::Quota {
                quota,
                used,
                needed,
            } => write!(
                f,
                "plugin storage quota of {quota} bytes exceeded ({used} used, {needed} more needed)"
            ),
            KvError::Key(k) => write!(f, "invalid key: {k}"),
        }
    }
}

impl std::error::Error for KvError {}

/// One native plugin command record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginCommand {
    pub id: String,
    pub plugin_id: String,
    pub status: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    /// The whole record (argv, context, source, exit code, tails…).
    pub record: Value,
}

fn check_key(key: &str) -> Result<(), KvError> {
    if key.is_empty() || key.len() > MAX_KEY || key.chars().any(|c| c.is_control()) {
        return Err(KvError::Key(format!(
            "keys are 1–{MAX_KEY} bytes without control characters"
        )));
    }
    Ok(())
}

impl Store {
    pub fn plugin_kv_get(&self, plugin: &str, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM plugin_kv WHERE plugin_id=?1 AND key=?2",
                params![plugin, key],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Bytes `plugin` holds (keys + values).
    pub fn plugin_kv_usage(&self, plugin: &str) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(length(key) + length(value)), 0) FROM plugin_kv WHERE plugin_id=?1",
            [plugin],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Set `key`; the outer `Result` is a database error, the inner one a limit refusal.
    pub fn plugin_kv_set(
        &self,
        plugin: &str,
        key: &str,
        value: &[u8],
        quota: u64,
    ) -> Result<Result<(), KvError>> {
        if let Err(e) = check_key(key) {
            return Ok(Err(e));
        }
        if value.len() > MAX_VALUE {
            return Ok(Err(KvError::ValueTooLarge(value.len())));
        }
        let tx = self.conn.unchecked_transaction()?;
        let used: i64 = tx.query_row(
            "SELECT COALESCE(SUM(length(key) + length(value)), 0) FROM plugin_kv WHERE plugin_id=?1 AND key<>?2",
            params![plugin, key],
            |r| r.get(0),
        )?;
        let needed = (key.len() + value.len()) as u64;
        if used as u64 + needed > quota {
            return Ok(Err(KvError::Quota {
                quota,
                used: used as u64,
                needed,
            }));
        }
        tx.execute(
            "INSERT INTO plugin_kv (plugin_id, key, value, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(plugin_id, key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            params![plugin, key, value, now_ms()],
        )?;
        tx.commit()?;
        Ok(Ok(()))
    }

    /// Delete `key`; true when it existed.
    pub fn plugin_kv_delete(&self, plugin: &str, key: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM plugin_kv WHERE plugin_id=?1 AND key=?2",
            params![plugin, key],
        )? > 0)
    }

    /// Keys of `plugin` starting with `prefix`, sorted, at most `limit`, after `after`.
    pub fn plugin_kv_list(
        &self,
        plugin: &str,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, usize)>> {
        let mut st = self.conn.prepare(
            "SELECT key, length(value) FROM plugin_kv WHERE plugin_id=?1 AND substr(key, 1, length(?2)) = ?2
             AND key > ?3 ORDER BY key LIMIT ?4",
        )?;
        let rows = st
            .query_map(
                params![plugin, prefix, after.unwrap_or(""), limit as i64],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Drop every key of `plugin` (plugin removal with `--purge`).
    pub fn plugin_kv_clear(&self, plugin: &str) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM plugin_kv WHERE plugin_id=?1", [plugin])?)
    }

    pub fn plugin_command_put(&self, c: &PluginCommand) -> Result<()> {
        self.conn.execute(
            "INSERT INTO plugin_commands (id, plugin_id, status, started_at, ended_at, json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET status=excluded.status, ended_at=excluded.ended_at, json=excluded.json",
            params![
                c.id,
                c.plugin_id,
                c.status,
                c.started_at,
                c.ended_at,
                serde_json::to_string(&c.record)?
            ],
        )?;
        Ok(())
    }

    /// Newest first; `plugin` filters.
    pub fn plugin_commands(&self, plugin: Option<&str>, limit: usize) -> Result<Vec<PluginCommand>> {
        let mut st = self.conn.prepare(
            "SELECT id, plugin_id, status, started_at, ended_at, json FROM plugin_commands
             WHERE (?1 IS NULL OR plugin_id = ?1) ORDER BY started_at DESC, id DESC LIMIT ?2",
        )?;
        let rows = st
            .query_map(params![plugin, limit as i64], |r| {
                Ok(PluginCommand {
                    id: r.get(0)?,
                    plugin_id: r.get(1)?,
                    status: r.get(2)?,
                    started_at: r.get(3)?,
                    ended_at: r.get(4)?,
                    record: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(Value::Null),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Keep the newest `keep` records per plugin (running ones are never dropped).
    pub fn plugin_commands_prune(&self, keep: usize) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM plugin_commands WHERE status <> 'running' AND id IN (
               SELECT id FROM (SELECT id, ROW_NUMBER() OVER (PARTITION BY plugin_id ORDER BY started_at DESC, id DESC) AS n
                               FROM plugin_commands) WHERE n > ?1)",
            [keep as i64],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kv_limits_quota_and_namespaces() {
        let s = Store::open_in_memory().unwrap();
        s.plugin_kv_set("a.b", "k", b"v1", DEFAULT_QUOTA).unwrap().unwrap();
        s.plugin_kv_set("a.b", "k", b"v2", DEFAULT_QUOTA).unwrap().unwrap();
        assert_eq!(s.plugin_kv_get("a.b", "k").unwrap(), Some(b"v2".to_vec()));
        assert_eq!(s.plugin_kv_get("c.d", "k").unwrap(), None, "per-plugin namespace");
        let big = vec![0u8; MAX_VALUE + 1];
        assert_eq!(
            s.plugin_kv_set("a.b", "big", &big, DEFAULT_QUOTA).unwrap(),
            Err(KvError::ValueTooLarge(MAX_VALUE + 1))
        );
        // Quota counts keys and values; replacing a key does not count it twice.
        s.plugin_kv_set("q.q", "x", &[1; 60], 100).unwrap().unwrap();
        s.plugin_kv_set("q.q", "x", &[2; 90], 100).unwrap().unwrap();
        assert!(matches!(
            s.plugin_kv_set("q.q", "y", &[3; 20], 100).unwrap(),
            Err(KvError::Quota { .. })
        ));
        assert_eq!(s.plugin_kv_usage("q.q").unwrap(), 91);
        assert!(s.plugin_kv_set("a.b", "", b"x", DEFAULT_QUOTA).unwrap().is_err());
        for k in ["p/1", "p/2", "q/1"] {
            s.plugin_kv_set("a.b", k, b"x", DEFAULT_QUOTA).unwrap().unwrap();
        }
        let l = s.plugin_kv_list("a.b", "p/", None, 10).unwrap();
        assert_eq!(l.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["p/1", "p/2"]);
        let l = s.plugin_kv_list("a.b", "", Some("p/2"), 10).unwrap();
        assert_eq!(l[0].0, "q/1");
        assert!(s.plugin_kv_delete("a.b", "p/1").unwrap());
        assert!(!s.plugin_kv_delete("a.b", "p/1").unwrap());
        assert_eq!(s.plugin_kv_clear("a.b").unwrap(), 3);
    }

    #[test]
    fn command_records_round_trip_and_prune() {
        let s = Store::open_in_memory().unwrap();
        for i in 0..5 {
            s.plugin_command_put(&PluginCommand {
                id: format!("c{i}"),
                plugin_id: "a.b".into(),
                status: if i == 0 { "running" } else { "completed" }.into(),
                started_at: i,
                ended_at: None,
                record: json!({"n": i}),
            })
            .unwrap();
        }
        let all = s.plugin_commands(Some("a.b"), 10).unwrap();
        assert_eq!(all[0].id, "c4", "newest first");
        assert_eq!(all[0].record["n"], 4);
        assert_eq!(s.plugin_commands_prune(2).unwrap(), 2);
        let left: Vec<String> = s
            .plugin_commands(None, 10)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(left, ["c4", "c3", "c0"], "running records are kept");
    }
}
