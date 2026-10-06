//! `vibeke forget` for the event log (09 §9.3): events in scope are replaced by tombstones
//! so `seq` stays gapless for subscribers resuming from a cursor. A tombstone keeps `seq`, `ts`
//! and `tier`; its type becomes `tombstone` and subject, actor and data are emptied. Records
//! of earlier forgets (`*.forgotten`) and existing tombstones are left alone. VT snapshots of
//! panes in scope can be dropped with [`Store::forget_snapshots`].

use crate::Store;
use anyhow::Result;
use rusqlite::params_from_iter;
use rusqlite::types::Value as Sql;
use serde::{Deserialize, Serialize};

/// Which events to tombstone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventScope {
    /// Every event.
    All,
    /// Events whose subject names one of these panes.
    Panes(Vec<String>),
    /// Events whose subject names this workspace or one of these panes.
    Workspace { id: String, panes: Vec<String> },
    /// Events recorded before this time (ms).
    Before(i64),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TombstoneReport {
    pub events: u64,
    pub snapshots: u64,
}

const KEEP: &str = "type <> 'tombstone' AND type NOT LIKE '%.forgotten'";

fn cond(scope: &EventScope) -> (String, Vec<Sql>) {
    let in_list = |col: &str, ids: &[String], args: &mut Vec<Sql>| -> String {
        if ids.is_empty() {
            return "0".into();
        }
        for i in ids {
            args.push(Sql::Text(i.clone()));
        }
        format!("{col} IN ({})", vec!["?"; ids.len()].join(","))
    };
    let mut args = Vec::new();
    let c = match scope {
        EventScope::All => "1".to_string(),
        EventScope::Before(t) => {
            args.push(Sql::Integer(*t));
            "ts < ?".to_string()
        }
        EventScope::Panes(ids) => in_list("json_extract(subject_json, '$.pane')", ids, &mut args),
        EventScope::Workspace { id, panes } => {
            args.push(Sql::Text(id.clone()));
            let p = in_list("json_extract(subject_json, '$.pane')", panes, &mut args);
            format!("(json_extract(subject_json, '$.workspace') = ? OR {p})")
        }
    };
    (
        format!("{KEEP} AND json_valid(COALESCE(subject_json, '{{}}')) AND {c}"),
        args,
    )
}

impl Store {
    /// Count (`dry_run`) or tombstone the events in `scope`. One transaction.
    pub fn tombstone_events(&self, scope: &EventScope, dry_run: bool) -> Result<u64> {
        let (c, args) = cond(scope);
        let n: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM events WHERE {c}"),
            params_from_iter(args.iter()),
            |r| r.get(0),
        )?;
        if !dry_run && n > 0 {
            self.conn.execute(
                &format!(
                    "UPDATE events SET type = 'tombstone', subject_json = '{{}}', actor_json = '{{}}', data_json = '{{}}' WHERE {c}"
                ),
                params_from_iter(args.iter()),
            )?;
        }
        Ok(n as u64)
    }

    /// Count (`dry_run`) or delete the VT snapshots of these panes (`None`: every pane) taken
    /// before `before` when given, never those of `keep` (live panes recover from them).
    pub fn forget_snapshots(
        &self,
        panes: Option<&[String]>,
        before: Option<i64>,
        keep: &[String],
        dry_run: bool,
    ) -> Result<u64> {
        let mut args: Vec<Sql> = Vec::new();
        let mut c = String::from("1");
        if !keep.is_empty() {
            c.push_str(&format!(
                " AND pane_id NOT IN ({})",
                vec!["?"; keep.len()].join(",")
            ));
            args.extend(keep.iter().map(|i| Sql::Text(i.clone())));
        }
        if let Some(ids) = panes {
            if ids.is_empty() {
                return Ok(0);
            }
            c.push_str(&format!(
                " AND pane_id IN ({})",
                vec!["?"; ids.len()].join(",")
            ));
            args.extend(ids.iter().map(|i| Sql::Text(i.clone())));
        }
        if let Some(t) = before {
            c.push_str(" AND taken_at < ?");
            args.push(Sql::Integer(t));
        }
        let n: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM vt_snapshots WHERE {c}"),
            params_from_iter(args.iter()),
            |r| r.get(0),
        )?;
        if !dry_run && n > 0 {
            self.conn.execute(
                &format!("DELETE FROM vt_snapshots WHERE {c}"),
                params_from_iter(args.iter()),
            )?;
        }
        Ok(n as u64)
    }
}

impl Store {
    /// Every `(key, value)` of a kv scope.
    pub fn kv_scan(&self, scope: &str) -> Result<Vec<(String, String)>> {
        let mut st = self
            .conn
            .prepare("SELECT key, value FROM kv WHERE scope = ?1 ORDER BY key")?;
        let v = st
            .query_map([scope], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutation;
    use serde_json::json;

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(&d.path().join("state.db")).unwrap();
        (d, s)
    }

    #[test]
    fn tombstones_keep_seq_and_drop_content() {
        let (_d, mut s) = store();
        let mut m = Mutation::default();
        m.event(
            "pane.output",
            json!({"pane": "p1", "workspace": "w1"}),
            json!({"text": "SECRET-1"}),
        );
        m.event(
            "pane.output",
            json!({"pane": "p2", "workspace": "w2"}),
            json!({"text": "keep"}),
        );
        m.event(
            "scrollback.forgotten",
            json!({"scope": {"pane": "p1"}}),
            json!({}),
        );
        m.event(
            "workspace.renamed",
            json!({"workspace": "w1"}),
            json!({"name": "SECRET-2"}),
        );
        s.commit(m).unwrap();
        let before = s.last_seq().unwrap();
        let scope = EventScope::Workspace {
            id: "w1".into(),
            panes: vec!["p1".into()],
        };
        assert_eq!(s.tombstone_events(&scope, true).unwrap(), 2);
        assert_eq!(s.tombstone_events(&scope, false).unwrap(), 2);
        // Idempotent; seq unchanged and gapless.
        assert_eq!(s.tombstone_events(&scope, false).unwrap(), 0);
        assert_eq!(s.last_seq().unwrap(), before);
        let all: Vec<(i64, String, String)> = {
            let mut st = s
                .conn
                .prepare("SELECT seq, type, data_json FROM events ORDER BY seq")
                .unwrap();
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let types: Vec<&str> = all.iter().map(|r| r.1.as_str()).collect();
        assert!(types.contains(&"tombstone"));
        assert!(types.contains(&"scrollback.forgotten"));
        assert!(!all.iter().any(|r| r.2.contains("SECRET")));
        assert!(all.iter().any(|r| r.2.contains("keep")));
        for w in all.windows(2) {
            assert_eq!(w[1].0, w[0].0 + 1);
        }
        assert_eq!(
            s.tombstone_events(&EventScope::Panes(vec![]), false)
                .unwrap(),
            0
        );
        assert!(s.tombstone_events(&EventScope::All, false).unwrap() >= 1);
    }

    #[test]
    fn snapshots_of_live_panes_are_kept() {
        let (_d, mut s) = store();
        let mut m = Mutation::default();
        m.snapshot("p1", 10, "e", "1", vec![1, 2, 3], "inc");
        m.snapshot("p2", 10, "e", "1", vec![4], "inc");
        s.commit(m).unwrap();
        let keep = vec!["p1".to_string()];
        assert_eq!(s.forget_snapshots(None, None, &keep, true).unwrap(), 1);
        assert_eq!(s.forget_snapshots(None, None, &keep, false).unwrap(), 1);
        assert_eq!(s.forget_snapshots(None, None, &[], true).unwrap(), 1);
        let ids = vec!["p1".to_string()];
        assert_eq!(
            s.forget_snapshots(Some(&ids), Some(0), &[], true).unwrap(),
            0
        );
    }
}
