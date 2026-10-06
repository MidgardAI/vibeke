//! Deleting archived scrollback and rebuilding its index (02 "Archive search as implemented").
//!
//! The zstd segments under `scrollback/<pane>/` are the source of truth for archived text;
//! `scrollback_fts` and `archive_panes` are derived from them. Retention, `forget` and
//! `doctor --rebuild-index` all go through here so the files and the index never disagree:
//! the matching FTS rows are deleted in the same SQLite transaction as the segment files, and
//! the transaction is rolled back when a file cannot be removed.

use crate::archive::{Archive, SegInfo, Select, read_seg_lossy};
use crate::{Store, now_ms};
use anyhow::Result;
use rusqlite::types::Value as Sql;
use rusqlite::{Params, params, params_from_iter};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// What a purge removed (or, for a dry run, would remove).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeReport {
    /// Panes that lost at least one segment.
    pub panes: u64,
    pub segments: u64,
    /// Compressed bytes of the deleted segments.
    pub bytes: u64,
    pub fts_rows: u64,
    /// `archive_panes` rows removed because the pane has no archive left.
    pub panes_dropped: u64,
}

/// Result of [`Store::rebuild_archive_index`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildReport {
    pub panes: u64,
    pub segments: u64,
    pub rows_indexed: u64,
    /// `scrollback_fts` rows dropped before re-indexing.
    pub fts_rows_before: u64,
    pub archive_panes_before: u64,
    pub archive_panes_after: u64,
    /// Segments that were damaged but yielded rows (indexed up to the damage).
    pub damaged: Vec<String>,
    /// Segments that yielded nothing and were left out of the index (files are untouched).
    pub skipped: Vec<String>,
}

const CHUNK: usize = 100;

impl Store {
    /// Purge archived scrollback. `panes: None` means every pane (on disk or in the index);
    /// `sel` picks the segments. Segment files and the FTS rows for their line ranges are
    /// deleted together; a pane left with no archive also loses its remaining FTS rows and its
    /// `archive_panes` row. `dry_run` only counts. Idempotent.
    pub fn purge_archive(
        &self,
        archive: &mut Archive,
        panes: Option<&[String]>,
        sel: Select,
        dry_run: bool,
    ) -> Result<PurgeReport> {
        let mut ids: Vec<String> = match panes {
            Some(p) => p.to_vec(),
            None => {
                let mut v = archive.pane_ids();
                if sel == Select::All {
                    v.extend(self.archive_pane_ids()?);
                }
                v
            }
        };
        ids.sort();
        ids.dedup();
        let mut report = PurgeReport::default();
        let mut ranges: Vec<(String, u64, u64)> = Vec::new();
        let mut chosen: Vec<SegInfo> = Vec::new();
        let mut emptied: Vec<String> = Vec::new();
        for id in &ids {
            let segs = archive.select(id, sel);
            if !segs.is_empty() {
                report.panes += 1;
            }
            let remaining = archive.segment_infos(id).len() - segs.len();
            let open_left = sel != Select::All && archive.is_open(id);
            if remaining == 0 && !open_left && (!segs.is_empty() || sel == Select::All) {
                emptied.push(id.clone());
            }
            for s in &segs {
                report.segments += 1;
                report.bytes += s.bytes;
                ranges.push((id.clone(), s.start, s.end));
            }
            chosen.extend(segs);
        }
        // An emptied pane is deleted by id below; its ranges would only be counted twice.
        ranges.retain(|(p, ..)| !emptied.contains(p));
        let whole_all = panes.is_none() && sel == Select::All;
        let tx = self.conn.unchecked_transaction()?;
        if whole_all {
            report.fts_rows += scalar(&tx, "SELECT COUNT(*) FROM scrollback_fts", [])?;
            report.panes_dropped += scalar(&tx, "SELECT COUNT(*) FROM archive_panes", [])?;
            if !dry_run {
                tx.execute("DELETE FROM scrollback_fts", [])?;
                tx.execute("DELETE FROM archive_panes", [])?;
            }
        } else {
            for chunk in ranges.chunks(CHUNK) {
                let (cond, args) = range_cond(chunk);
                report.fts_rows += scalar(
                    &tx,
                    &format!("SELECT COUNT(*) FROM scrollback_fts WHERE {cond}"),
                    params_from_iter(args.iter()),
                )?;
                if !dry_run {
                    tx.execute(
                        &format!("DELETE FROM scrollback_fts WHERE {cond}"),
                        params_from_iter(args.iter()),
                    )?;
                }
            }
            for chunk in emptied.chunks(CHUNK) {
                let marks = vec!["?"; chunk.len()].join(",");
                // The pane's archive is gone: nothing may stay searchable under its id.
                report.fts_rows += scalar(
                    &tx,
                    &format!("SELECT COUNT(*) FROM scrollback_fts WHERE pane_id IN ({marks})"),
                    params_from_iter(chunk.iter()),
                )?;
                report.panes_dropped += scalar(
                    &tx,
                    &format!("SELECT COUNT(*) FROM archive_panes WHERE pane_id IN ({marks})"),
                    params_from_iter(chunk.iter()),
                )?;
                if !dry_run {
                    tx.execute(
                        &format!("DELETE FROM scrollback_fts WHERE pane_id IN ({marks})"),
                        params_from_iter(chunk.iter()),
                    )?;
                    tx.execute(
                        &format!("DELETE FROM archive_panes WHERE pane_id IN ({marks})"),
                        params_from_iter(chunk.iter()),
                    )?;
                }
            }
        }
        if dry_run {
            return Ok(report);
        }
        if sel == Select::All {
            for id in &ids {
                archive.drop_open(id);
            }
        }
        // Files after the index rows, before the commit: a failure rolls the rows back.
        Archive::remove_segments(&chosen)?;
        tx.commit()?;
        for id in &emptied {
            archive.prune_dir(id);
        }
        Ok(report)
    }

    fn archive_pane_ids(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT pane_id FROM archive_panes")?;
        let v = st
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Panes the index knows to belong to `workspace` (closed panes included).
    pub fn archive_panes_in_workspace(&self, workspace: &str) -> Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare("SELECT pane_id FROM archive_panes WHERE workspace = ?1")?;
        let v = st
            .query_map([workspace], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Rebuild `scrollback_fts` and `archive_panes` from the segments under `root`
    /// (`scrollback/`). Call with the server stopped. Everything happens in one transaction, so
    /// an error leaves the old index. A damaged segment is indexed up to the damage and listed;
    /// one with no readable rows is skipped and listed. Segment files are never modified.
    ///
    /// Rows carry no timestamp on disk, so `ts` becomes the segment file's modification time.
    /// `archive_panes` keeps rows for panes that still have segments, adds missing ones from the
    /// pane entities in `state.db` when present, and drops rows for panes with no segments.
    pub fn rebuild_archive_index(&self, root: &Path) -> Result<RebuildReport> {
        let archive = Archive::new(root);
        let mut rep = RebuildReport {
            fts_rows_before: scalar(&self.conn, "SELECT COUNT(*) FROM scrollback_fts", [])?,
            archive_panes_before: scalar(&self.conn, "SELECT COUNT(*) FROM archive_panes", [])?,
            ..Default::default()
        };
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM scrollback_fts", [])?;
        let mut with_data: Vec<String> = Vec::new();
        {
            let mut ins = tx.prepare(
                "INSERT INTO scrollback_fts (pane_id, line_no, ts, text) VALUES (?1,?2,?3,?4)",
            )?;
            for pane in archive.pane_ids() {
                let segs = archive.segment_infos(&pane);
                if segs.is_empty() {
                    continue;
                }
                for s in &segs {
                    rep.segments += 1;
                    let name = format!("{pane}/{}", s.path.file_name().unwrap().to_string_lossy());
                    let read = match read_seg_lossy(&s.path) {
                        Ok(r) => r,
                        Err(_) => {
                            rep.skipped.push(name);
                            continue;
                        }
                    };
                    if read.rows.is_empty() {
                        if read.damaged || s.bytes > 0 {
                            rep.skipped.push(name);
                        }
                        continue;
                    }
                    if read.damaged {
                        rep.damaged.push(name);
                    }
                    for r in read.rows.iter().filter(|r| !r.t.is_empty()) {
                        ins.execute(params![pane, r.n as i64, s.mtime_ms, r.t])?;
                        rep.rows_indexed += 1;
                    }
                }
                rep.panes += 1;
                with_data.push(pane);
            }
        }
        let known: Vec<String> = {
            let mut st = tx.prepare("SELECT pane_id FROM archive_panes")?;
            st.query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for id in known.iter().filter(|k| !with_data.contains(k)) {
            tx.execute("DELETE FROM archive_panes WHERE pane_id = ?1", [id])?;
        }
        for id in with_data.iter().filter(|p| !known.contains(p)) {
            let ent: Option<String> = tx
                .query_row(
                    "SELECT json FROM entities WHERE kind='pane' AND id=?1",
                    [id],
                    |r| r.get(0),
                )
                .ok();
            let Some(json) = ent else { continue };
            let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
            let text = |k: &str| {
                v.get(k)
                    .and_then(|x| x.as_str())
                    .filter(|t| !t.is_empty())
                    .map(str::to_string)
            };
            tx.execute(
                "INSERT INTO archive_panes (pane_id, workspace, tab, handle, title, updated_at) VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    id,
                    text("workspace"),
                    text("tab"),
                    text("handle"),
                    text("title").or_else(|| text("auto_title")),
                    now_ms()
                ],
            )?;
        }
        rep.archive_panes_after = scalar(&tx, "SELECT COUNT(*) FROM archive_panes", [])?;
        tx.commit()?;
        Ok(rep)
    }
}

fn scalar(c: &rusqlite::Connection, sql: &str, args: impl Params) -> Result<u64> {
    Ok(c.query_row(sql, args, |r| r.get::<_, i64>(0))? as u64)
}

fn range_cond(chunk: &[(String, u64, u64)]) -> (String, Vec<Sql>) {
    // `end` can be u64::MAX (the last segment): SQLite integers are i64.
    let mut args = Vec::new();
    let cond = chunk
        .iter()
        .map(|(p, a, b)| {
            args.push(Sql::Text(p.clone()));
            args.push(Sql::Integer((*a).min(i64::MAX as u64) as i64));
            args.push(Sql::Integer((*b).min(i64::MAX as u64) as i64));
            "(pane_id = ? AND line_no >= ? AND line_no < ?)"
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    (cond, args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::ArchivedRow;

    /// Two panes; `p1` has several segments (rotation at 1 MiB uncompressed).
    fn fixture(dir: &Path) -> (Store, Archive) {
        let s = Store::open_in_memory().unwrap();
        let mut a = Archive::new(dir);
        for pane in ["p1", "p2"] {
            let chunks = if pane == "p1" { 30u64 } else { 1 };
            let mut fts = Vec::new();
            for chunk in 0..chunks {
                let rows: Vec<ArchivedRow> = (0..1000)
                    .map(|i| ArchivedRow {
                        n: chunk * 1000 + i,
                        t: format!("needle {pane} {} {}", chunk * 1000 + i, "x".repeat(40)),
                        w: false,
                    })
                    .collect();
                fts.extend(
                    rows.iter()
                        .map(|r| (pane.to_string(), r.n, 1_000, r.t.clone())),
                );
                a.append(pane, &rows).unwrap();
                a.flush().unwrap();
            }
            s.fts_insert(&fts).unwrap();
        }
        s.fts_register_panes(&[
            (
                "p1".into(),
                "w1".into(),
                "t1".into(),
                "w1:p1".into(),
                "one".into(),
            ),
            (
                "p2".into(),
                "w2".into(),
                "t2".into(),
                "w2:p2".into(),
                "two".into(),
            ),
        ])
        .unwrap();
        (s, a)
    }

    fn fts_count(s: &Store, pane: &str) -> i64 {
        s.conn
            .query_row(
                "SELECT COUNT(*) FROM scrollback_fts WHERE pane_id=?1",
                [pane],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn retention_deletes_matching_fts_rows() {
        let d = tempfile::tempdir().unwrap();
        let (s, mut a) = fixture(d.path());
        let segs = a.segment_infos("p1");
        assert!(segs.len() > 2);
        let dry = s
            .purge_archive(&mut a, Some(&["p1".into()]), Select::OverBytes(1), true)
            .unwrap();
        assert!(dry.segments > 0);
        assert_eq!(fts_count(&s, "p1"), 30_000, "dry run deletes nothing");
        let r = s
            .purge_archive(&mut a, Some(&["p1".into()]), Select::OverBytes(1), false)
            .unwrap();
        assert_eq!(r.segments, dry.segments);
        assert_eq!(r.fts_rows, dry.fts_rows);
        // Rows of deleted segments are gone, rows of the kept (newest) segment stay.
        let left = a.segment_infos("p1");
        assert_eq!(left.len() as u64, segs.len() as u64 - r.segments);
        assert_eq!(fts_count(&s, "p1") as u64, 30_000 - r.fts_rows);
        let first_kept = left[0].start as i64;
        let min_line: i64 = s
            .conn
            .query_row(
                "SELECT MIN(line_no) FROM scrollback_fts WHERE pane_id='p1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(min_line, first_kept);
        assert_eq!(fts_count(&s, "p2"), 1000, "other panes untouched");
        assert!(
            s.archive_pane("p1").unwrap().is_some(),
            "pane keeps its segment"
        );
        // Idempotent.
        let again = s
            .purge_archive(&mut a, Some(&["p1".into()]), Select::OverBytes(1), false)
            .unwrap();
        assert_eq!(again, PurgeReport::default());
    }

    #[test]
    fn age_retention_drops_pane_with_no_segments_left() {
        let d = tempfile::tempdir().unwrap();
        let (s, mut a) = fixture(d.path());
        a.close_pane("p2").unwrap();
        let future = crate::now_ms() + 60_000;
        let r = s
            .purge_archive(
                &mut a,
                Some(&["p2".into()]),
                Select::OlderThan(future),
                false,
            )
            .unwrap();
        assert_eq!((r.segments, r.fts_rows, r.panes_dropped), (1, 1000, 1));
        assert_eq!(fts_count(&s, "p2"), 0);
        assert!(s.archive_pane("p2").unwrap().is_none());
        assert!(!d.path().join("p2").exists());
        // The open segment of p1 is never taken by age retention.
        let r = s
            .purge_archive(
                &mut a,
                Some(&["p1".into()]),
                Select::OlderThan(future),
                false,
            )
            .unwrap();
        assert!(r.segments > 0);
        assert!(fts_count(&s, "p1") > 0);
        assert!(s.archive_pane("p1").unwrap().is_some());
    }

    #[test]
    fn forget_all_and_scoped() {
        let d = tempfile::tempdir().unwrap();
        let (s, mut a) = fixture(d.path());
        let ws = s.archive_panes_in_workspace("w2").unwrap();
        assert_eq!(ws, vec!["p2".to_string()]);
        let r = s
            .purge_archive(&mut a, Some(&ws), Select::All, false)
            .unwrap();
        assert_eq!((r.panes, r.fts_rows, r.panes_dropped), (1, 1000, 1));
        assert!(a.read("p2", 0, 10).unwrap().is_empty());
        assert_eq!(fts_count(&s, "p1"), 30_000);
        let r = s.purge_archive(&mut a, None, Select::All, false).unwrap();
        assert_eq!(r.fts_rows, 30_000);
        assert_eq!(fts_count(&s, "p1"), 0);
        assert!(a.pane_ids().is_empty());
        assert!(s.archive_pane("p1").unwrap().is_none());
        // Idempotent; appends after a forget of a live pane start a fresh segment.
        let r = s.purge_archive(&mut a, None, Select::All, false).unwrap();
        assert_eq!(r, PurgeReport::default());
        a.append(
            "p1",
            &[ArchivedRow {
                n: 31_000,
                t: "after".into(),
                w: false,
            }],
        )
        .unwrap();
        a.flush().unwrap();
        assert_eq!(a.read("p1", 0, u64::MAX).unwrap().len(), 1);
    }

    #[test]
    fn rebuild_restores_index_and_reports_corrupt_segments() {
        let d = tempfile::tempdir().unwrap();
        let (s, mut a) = fixture(d.path());
        a.close_pane("p1").unwrap();
        a.close_pane("p2").unwrap();
        let p1 = a.segment_infos("p1");
        // Corrupt one whole segment and truncate another.
        std::fs::write(&p1[0].path, b"not zstd at all").unwrap();
        let raw = std::fs::read(&p1[1].path).unwrap();
        std::fs::write(&p1[1].path, &raw[..raw.len() / 2]).unwrap();
        // Wreck the derived tables.
        s.conn.execute("DELETE FROM scrollback_fts", []).unwrap();
        s.conn.execute("DELETE FROM archive_panes", []).unwrap();
        s.conn
            .execute(
                "INSERT INTO scrollback_fts (pane_id, line_no, ts, text) VALUES ('ghost', 1, 1, 'ghost row')",
                [],
            )
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO archive_panes (pane_id, updated_at) VALUES ('ghost', 1)",
                [],
            )
            .unwrap();
        let rep = s.rebuild_archive_index(d.path()).unwrap();
        assert_eq!(rep.fts_rows_before, 1);
        assert_eq!(rep.panes, 2);
        assert_eq!(rep.segments as usize, p1.len() + 1);
        assert_eq!(rep.skipped.len(), 1, "{rep:?}");
        assert!(rep.skipped[0].starts_with("p1/"));
        assert_eq!(rep.damaged.len(), 1, "{rep:?}");
        assert!(rep.damaged[0].ends_with(&format!("{:016}.zst", p1[1].start)));
        assert_eq!(fts_count(&s, "ghost"), 0);
        assert_eq!(fts_count(&s, "p2"), 1000);
        assert!(fts_count(&s, "p1") > 0);
        assert_eq!(rep.rows_indexed as i64, fts_count(&s, "p1") + 1000);
        let hits = s.fts_search("needle p2", Some("p2"), 5).unwrap();
        assert_eq!(hits.len(), 5);
        assert_eq!(
            rep.archive_panes_after, 0,
            "no pane entities to restore from"
        );
        assert!(s.archive_pane("ghost").unwrap().is_none());
        // Idempotent.
        let rep2 = s.rebuild_archive_index(d.path()).unwrap();
        assert_eq!(rep2.rows_indexed, rep.rows_indexed);
        assert_eq!(rep2.fts_rows_before, rep.rows_indexed);
    }

    #[test]
    fn rebuild_restores_archive_panes_from_pane_entities() {
        let d = tempfile::tempdir().unwrap();
        let (mut s, mut a) = fixture(d.path());
        a.close_pane("p2").unwrap();
        let mut m = crate::Mutation::new();
        m.put(
            "pane",
            "p2",
            Some("w2:p2"),
            &serde_json::json!({"id":"p2","workspace":"w2","tab":"t2","handle":"w2:p2","title":null,"auto_title":"zsh"}),
        );
        s.commit(m).unwrap();
        s.conn.execute("DELETE FROM archive_panes", []).unwrap();
        s.rebuild_archive_index(d.path()).unwrap();
        let p = s.archive_pane("p2").unwrap().unwrap();
        assert_eq!((p.1.as_str(), p.4.as_str()), ("w2", "zsh"));
        assert!(s.archive_pane("p1").unwrap().is_none());
    }
}
