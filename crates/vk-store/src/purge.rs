//! Deleting archived scrollback and rebuilding its index (02 "Archive search as implemented").
//!
//! The zstd segments under `scrollback/<pane>/` are the source of truth for archived text;
//! `scrollback_fts` and `archive_panes` are derived from them. Retention, `forget` and
//! `doctor --rebuild-index` all go through here so the files and the index never disagree:
//! segments are first moved into a staging dir (`scrollback/.trash/<purge-id>/`), then the
//! matching FTS rows and a purge-journal row commit in one SQLite transaction, then the staging
//! dir is unlinked. A failure before the commit moves the segments back; a crash is settled at
//! startup by [`Store::recover_archive_purges`] according to whether the journal row committed.

use crate::archive::{Archive, SegInfo, Select, read_seg_lossy};
use crate::{Store, now_ms};
use anyhow::Result;
use rusqlite::types::Value as Sql;
use rusqlite::{Params, params, params_from_iter};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
        // Recoverable deletion: (1) record the purge in the journal and move the segments into
        // `.trash/<purge-id>/` (a failure moves them back and rolls the index back); (2) commit
        // the index rows and the journal row together; (3) unlink the staging dir and the
        // journal row. A crash in between is settled by `recover_archive_purges` at startup:
        // journal row committed → the staging dir goes; not committed → its files come back.
        let staging = if chosen.is_empty() {
            None
        } else {
            let purge_id = ulid::Ulid::new().to_string();
            tx.execute(
                "INSERT INTO kv (scope, key, value) VALUES (?1, ?2, ?3)",
                params![PURGE_JOURNAL, purge_id, now_ms().to_string()],
            )?;
            let dir = archive.root().join(TRASH).join(&purge_id);
            let moved = stage(&chosen, &dir)?;
            Some((purge_id, dir, moved))
        };
        if let Err(e) = fault("commit")
            .map_err(anyhow::Error::from)
            .and_then(|()| tx.commit().map_err(anyhow::Error::from))
        {
            if let Some((_, dir, moved)) = &staging {
                unstage(moved, dir);
            }
            return Err(e);
        }
        fault("crash_after_commit")?;
        if sel == Select::All {
            for id in &ids {
                archive.drop_open(id);
            }
        }
        for id in &emptied {
            archive.prune_dir(id);
        }
        if let Some((purge_id, dir, _)) = &staging {
            // Committed: the purge has happened. A staging dir that can't be removed now is
            // removed at the next startup (the journal row stays until it is).
            if fault("unlink").is_ok() && std::fs::remove_dir_all(dir).is_ok() {
                let _ = self.conn.execute(
                    "DELETE FROM kv WHERE scope = ?1 AND key = ?2",
                    params![PURGE_JOURNAL, purge_id],
                );
                let _ = std::fs::remove_dir(archive.root().join(TRASH));
            }
        }
        Ok(report)
    }

    /// Settle purges interrupted by a crash or a failed unlink (call at startup, before the
    /// archive is used). For each `.trash/<purge-id>/`: if the purge's journal row is in the
    /// database its transaction committed, so the staged segments are deleted; otherwise the
    /// index still has their rows, so the segments move back (a segment whose original path
    /// is taken meanwhile stays staged and is reported). Journal rows without a staging dir
    /// are dropped.
    pub fn recover_archive_purges(&self, root: &Path) -> Result<PurgeRecovery> {
        let mut rep = PurgeRecovery::default();
        let trash = root.join(TRASH);
        let committed: Vec<String> = {
            let mut st = self.conn.prepare("SELECT key FROM kv WHERE scope = ?1")?;
            st.query_map([PURGE_JOURNAL], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        let dirs: Vec<(String, std::path::PathBuf)> = std::fs::read_dir(&trash)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().into_string().ok().map(|n| (n, e.path())))
                    .collect()
            })
            .unwrap_or_default();
        for (id, dir) in &dirs {
            if committed.contains(id) {
                std::fs::remove_dir_all(dir)?;
                rep.completed += 1;
                continue;
            }
            let mut kept = false;
            for pane in std::fs::read_dir(dir)?.flatten() {
                let pane_name = pane.file_name();
                for seg in std::fs::read_dir(pane.path())?.flatten() {
                    let dest = root.join(&pane_name).join(seg.file_name());
                    if dest.exists() {
                        kept = true;
                        rep.conflicts.push(dest.display().to_string());
                        continue;
                    }
                    std::fs::create_dir_all(root.join(&pane_name))?;
                    std::fs::rename(seg.path(), &dest)?;
                    rep.segments_restored += 1;
                }
            }
            if !kept {
                std::fs::remove_dir_all(dir)?;
            }
            rep.restored += 1;
        }
        let present: Vec<&String> = dirs.iter().map(|(id, _)| id).collect();
        for id in committed.iter().filter(|id| !present.contains(id)) {
            self.conn.execute(
                "DELETE FROM kv WHERE scope = ?1 AND key = ?2",
                params![PURGE_JOURNAL, id],
            )?;
        }
        for (id, _) in dirs.iter().filter(|(id, _)| committed.contains(id)) {
            self.conn.execute(
                "DELETE FROM kv WHERE scope = ?1 AND key = ?2",
                params![PURGE_JOURNAL, id],
            )?;
        }
        let _ = std::fs::remove_dir(&trash);
        Ok(rep)
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
        self.rebuild_archive_index_with(root, &|t| t.to_string())
    }

    /// [`Store::rebuild_archive_index`] with each row's text passed through `index_text` before
    /// it is indexed (`security.redact_scrollback_index`, 09 §9.2: the index holds redacted
    /// text while the segments keep what the terminal showed).
    pub fn rebuild_archive_index_with(
        &self,
        root: &Path,
        index_text: &dyn Fn(&str) -> String,
    ) -> Result<RebuildReport> {
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
                        ins.execute(params![pane, r.n as i64, s.mtime_ms, index_text(&r.t)])?;
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

/// Staging area for segments being purged, inside `scrollback/` (same filesystem, so moves
/// are renames; dot directories are not panes).
pub const TRASH: &str = ".trash";
/// `kv` scope of the purge journal: one row per purge whose transaction committed and whose
/// staging dir may still exist.
const PURGE_JOURNAL: &str = "archive_purge";

/// What [`Store::recover_archive_purges`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeRecovery {
    /// Committed purges whose staged segments were deleted.
    pub completed: u64,
    /// Uncommitted purges whose segments were moved back.
    pub restored: u64,
    pub segments_restored: u64,
    /// Staged segments left in place because their original path is taken.
    pub conflicts: Vec<String>,
}

/// Move `segs` into `dir/<pane>/<file>`; on failure move back what was moved and fail.
fn stage(segs: &[SegInfo], dir: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
    let res = (|| -> std::io::Result<()> {
        for s in segs {
            let (Some(name), Some(pane)) = (
                s.path.file_name(),
                s.path.parent().and_then(|p| p.file_name()),
            ) else {
                continue;
            };
            let to = dir.join(pane).join(name);
            std::fs::create_dir_all(dir.join(pane))?;
            fault("stage")?;
            match std::fs::rename(&s.path, &to) {
                Ok(()) => moved.push((s.path.clone(), to)),
                // A missing file counts as deleted.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })();
    match res {
        Ok(()) => Ok(moved),
        Err(e) => {
            unstage(&moved, dir);
            Err(e.into())
        }
    }
}

/// Move staged segments back (newest move first) and drop the staging dir.
fn unstage(moved: &[(PathBuf, PathBuf)], dir: &Path) {
    for (from, to) in moved.iter().rev() {
        if let Err(e) = std::fs::rename(to, from) {
            eprintln!(
                "archive purge: could not restore staged segment {}: {e}; it is restored at the next start",
                from.display()
            );
            return;
        }
    }
    let _ = std::fs::remove_dir_all(dir);
    if let Some(trash) = dir.parent() {
        let _ = std::fs::remove_dir(trash);
    }
}

// Fault injection for the purge protocol's tests: `inject_fault(point, skip, crash)` makes the
// `skip`+1-th pass through `point` fail (or panic, for a simulated crash) on this thread.
#[cfg(test)]
thread_local! {
    static FAULT: std::cell::RefCell<Option<(&'static str, usize, bool)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn inject_fault(point: &'static str, skip: usize, crash: bool) {
    FAULT.with(|f| *f.borrow_mut() = Some((point, skip, crash)));
}

fn fault(point: &'static str) -> std::io::Result<()> {
    #[cfg(test)]
    {
        let hit = FAULT.with(|f| {
            let mut f = f.borrow_mut();
            match f.as_mut() {
                Some((p, n, crash)) if *p == point => {
                    if *n == 0 {
                        let crash = *crash;
                        *f = None;
                        Some(crash)
                    } else {
                        *n -= 1;
                        None
                    }
                }
                _ => None,
            }
        });
        match hit {
            Some(true) => panic!("injected crash at {point}"),
            Some(false) => {
                return Err(std::io::Error::other(format!(
                    "injected failure at {point}"
                )));
            }
            None => {}
        }
    }
    let _ = point;
    Ok(())
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

    // ---- recoverable deletion (leftovers review finding 8) ----

    /// Every segment file of `pane` with its bytes.
    fn files(a: &Archive, pane: &str) -> Vec<(PathBuf, Vec<u8>)> {
        a.segment_infos(pane)
            .into_iter()
            .map(|s| {
                let b = std::fs::read(&s.path).unwrap();
                (s.path, b)
            })
            .collect()
    }

    fn journal(s: &Store) -> i64 {
        s.conn
            .query_row(
                "SELECT COUNT(*) FROM kv WHERE scope = ?1",
                [PURGE_JOURNAL],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn trash_dirs(root: &Path) -> usize {
        std::fs::read_dir(root.join(TRASH))
            .map(|rd| rd.count())
            .unwrap_or(0)
    }

    /// The second segment can't be moved: the first comes back, the index rolls back, nothing
    /// is lost and nothing is staged.
    #[test]
    fn failure_on_the_second_segment_restores_the_first() {
        let d = tempfile::tempdir().unwrap();
        let (s, mut a) = fixture(d.path());
        a.close_pane("p1").unwrap();
        let before = files(&a, "p1");
        assert!(before.len() > 2);
        inject_fault("stage", 1, false);
        let err = s
            .purge_archive(&mut a, Some(&["p1".into()]), Select::OverBytes(1), false)
            .unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        assert_eq!(files(&a, "p1"), before);
        assert_eq!(fts_count(&s, "p1"), 30_000);
        assert_eq!(journal(&s), 0);
        assert!(!d.path().join(TRASH).exists());
        assert_eq!(a.pane_ids(), vec!["p1".to_string(), "p2".to_string()]);
        // Forget (Select::All) of a live pane: same, and its open segment keeps working.
        let live_before = files(&a, "p2");
        inject_fault("stage", 0, false);
        assert!(
            s.purge_archive(&mut a, Some(&["p2".into()]), Select::All, false)
                .is_err()
        );
        assert_eq!(files(&a, "p2"), live_before);
        assert_eq!(fts_count(&s, "p2"), 1000);
    }

    /// The commit fails after every segment was staged: all of them come back.
    #[test]
    fn commit_failure_restores_the_segments() {
        let d = tempfile::tempdir().unwrap();
        let (s, mut a) = fixture(d.path());
        let before = files(&a, "p1");
        inject_fault("commit", 0, false);
        assert!(s.purge_archive(&mut a, None, Select::All, false).is_err());
        assert_eq!(files(&a, "p1"), before);
        assert_eq!(fts_count(&s, "p1"), 30_000);
        assert_eq!(fts_count(&s, "p2"), 1000);
        assert_eq!(journal(&s), 0);
        assert!(!d.path().join(TRASH).exists());
        assert!(s.archive_pane("p1").unwrap().is_some());
        // The buffered (open) segment state survived too: appends land in the same file.
        a.append(
            "p1",
            &[ArchivedRow {
                n: 30_000,
                t: "later".into(),
                w: false,
            }],
        )
        .unwrap();
        a.flush().unwrap();
        assert_eq!(a.segment_infos("p1").len(), before.len());
        assert_eq!(a.read("p1", 30_000, 30_001).unwrap()[0].t, "later");
    }

    /// Unlinking the staging dir fails after the commit: the purge stands (the segments are no
    /// longer readable), and the next startup removes the staged files and the journal row.
    #[test]
    fn unlink_failure_after_commit_is_finished_at_startup() {
        let d = tempfile::tempdir().unwrap();
        let (s, mut a) = fixture(d.path());
        a.close_pane("p2").unwrap();
        inject_fault("unlink", 0, false);
        let r = s
            .purge_archive(&mut a, Some(&["p2".into()]), Select::All, false)
            .unwrap();
        assert_eq!(r.fts_rows, 1000);
        assert!(a.segment_infos("p2").is_empty());
        assert!(a.read("p2", 0, u64::MAX).unwrap().is_empty());
        assert!(!a.pane_ids().contains(&".trash".to_string()));
        assert_eq!((journal(&s), trash_dirs(d.path())), (1, 1));
        let rec = s.recover_archive_purges(d.path()).unwrap();
        assert_eq!((rec.completed, rec.restored), (1, 0));
        assert_eq!(journal(&s), 0);
        assert!(!d.path().join(TRASH).exists());
        assert_eq!(fts_count(&s, "p2"), 0);
        assert_eq!(fts_count(&s, "p1"), 30_000);
    }

    /// The process dies after staging, before the commit: at restart the index still has the
    /// rows (the transaction never committed), so the staged segments move back.
    #[test]
    fn restart_after_a_crash_before_the_commit_restores() {
        let d = tempfile::tempdir().unwrap();
        let (s, mut a) = fixture(d.path());
        a.close_pane("p1").unwrap();
        let before = files(&a, "p1");
        inject_fault("commit", 0, true);
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            s.purge_archive(&mut a, Some(&["p1".into()]), Select::OverBytes(1), false)
        }));
        assert!(crashed.is_err());
        assert!(files(&a, "p1").len() < before.len(), "segments are staged");
        assert_eq!(fts_count(&s, "p1"), 30_000, "nothing committed");
        assert_eq!((journal(&s), trash_dirs(d.path())), (0, 1));
        // Restart.
        let rec = s.recover_archive_purges(d.path()).unwrap();
        assert_eq!((rec.completed, rec.restored), (0, 1));
        assert!(rec.segments_restored > 0 && rec.conflicts.is_empty());
        let mut a = Archive::new(d.path());
        assert_eq!(files(&a, "p1"), before);
        assert_eq!(a.read("p1", 0, 3).unwrap().len(), 3);
        assert!(!d.path().join(TRASH).exists());
        // Retention then works normally.
        let r = s
            .purge_archive(&mut a, Some(&["p1".into()]), Select::OverBytes(1), false)
            .unwrap();
        assert!(r.segments > 0);
        assert_eq!((journal(&s), trash_dirs(d.path())), (0, 0));
    }

    /// The process dies right after the commit: at restart the journal row says the purge
    /// happened, so the staged segments are deleted, not restored.
    #[test]
    fn restart_after_a_crash_after_the_commit_completes() {
        let d = tempfile::tempdir().unwrap();
        let (s, mut a) = fixture(d.path());
        a.close_pane("p1").unwrap();
        inject_fault("crash_after_commit", 0, true);
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            s.purge_archive(&mut a, Some(&["p1".into()]), Select::All, false)
        }));
        assert!(crashed.is_err());
        assert_eq!(fts_count(&s, "p1"), 0, "committed");
        assert_eq!((journal(&s), trash_dirs(d.path())), (1, 1));
        let rec = s.recover_archive_purges(d.path()).unwrap();
        assert_eq!((rec.completed, rec.restored), (1, 0));
        assert!(Archive::new(d.path()).segment_infos("p1").is_empty());
        assert_eq!((journal(&s), trash_dirs(d.path())), (0, 0));
        assert_eq!(fts_count(&s, "p2"), 1000);
    }
}
