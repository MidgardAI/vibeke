//! Pre-migration backups of `state.db` (02 §3): before a forward-only migration runs, a
//! consistent copy (`VACUUM INTO`) of the database is written to `<state>/backups/`, and only the
//! last [`KEEP_BACKUPS`] are kept. Restoring a backup always rotates `log_epoch`, so a client
//! cursor from before the restore is never interpreted against the restored log.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::Serialize;
use std::path::{Path, PathBuf};

pub const KEEP_BACKUPS: usize = 3;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BackupInfo {
    pub name: String,
    pub path: PathBuf,
    /// Schema version the backup holds (what the database was before the migration).
    pub schema_version: i64,
    pub created_at_ms: i64,
    pub bytes: u64,
}

pub fn backup_dir(db: &Path) -> PathBuf {
    db.parent()
        .unwrap_or_else(|| Path::new("."))
        .join("backups")
}

/// `state-v<version>-<ms>.db`.
fn parse_name(name: &str) -> Option<(i64, i64)> {
    let rest = name.strip_prefix("state-v")?.strip_suffix(".db")?;
    let (v, ts) = rest.split_once('-')?;
    Some((v.parse().ok()?, ts.parse().ok()?))
}

/// Write a consistent copy of the open database and drop all but the newest [`KEEP_BACKUPS`].
pub fn make_backup(conn: &Connection, db: &Path, schema_version: i64) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = backup_dir(db);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let mut ts = crate::now_ms();
    let mut path = dir.join(format!("state-v{schema_version}-{ts}.db"));
    while path.exists() {
        ts += 1;
        path = dir.join(format!("state-v{schema_version}-{ts}.db"));
    }
    let target = path.to_string_lossy().into_owned();
    conn.execute("VACUUM INTO ?1", [target.as_str()])
        .with_context(|| format!("write backup {}", path.display()))?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    prune_backups(db, KEEP_BACKUPS);
    Ok(path)
}

/// Backups of `db`, newest first.
pub fn list_backups(db: &Path) -> Vec<BackupInfo> {
    let dir = backup_dir(db);
    let mut out: Vec<BackupInfo> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let (v, ts) = parse_name(&name)?;
            Some(BackupInfo {
                bytes: e.metadata().map(|m| m.len()).unwrap_or(0),
                path: e.path(),
                name,
                schema_version: v,
                created_at_ms: ts,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.created_at_ms
            .cmp(&a.created_at_ms)
            .then_with(|| b.name.cmp(&a.name))
    });
    out
}

/// Keep the newest `keep` backups. Returns how many were removed.
pub fn prune_backups(db: &Path, keep: usize) -> usize {
    let mut n = 0;
    for b in list_backups(db).into_iter().skip(keep) {
        if std::fs::remove_file(&b.path).is_ok() {
            n += 1;
        }
    }
    n
}

/// `state.forget` (09 §8): backups are whole-database copies (`VACUUM INTO`) that cannot be
/// filtered, and every existing one predates the forget, so all of them (and a kept
/// `state.db.pre-restore`) still hold the forgotten rows. Delete them. Returns how many files
/// were removed.
pub fn forget_backups(db: &Path) -> usize {
    let mut n = prune_backups(db, 0);
    let mut keep = db.as_os_str().to_owned();
    keep.push(".pre-restore");
    if std::fs::remove_file(PathBuf::from(keep)).is_ok() {
        n += 1;
    }
    n
}

/// Replace `db` with the backup file `backup` (a name from [`list_backups`] or a path) and rotate
/// `log_epoch`. The database being replaced is kept next to it as `state.db.pre-restore`. The
/// caller guarantees no server has the database open (the state lock).
pub fn restore_backup(db: &Path, backup: &str) -> Result<BackupInfo> {
    let info = list_backups(db)
        .into_iter()
        .find(|b| b.name == backup || b.path == Path::new(backup))
        .with_context(|| format!("no backup `{backup}` for {}", db.display()))?;
    // The copy is checked before anything of the live database is touched.
    let check =
        Connection::open_with_flags(&info.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let ok: String = check.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if ok != "ok" {
        bail!("backup {} fails its integrity check: {ok}", info.name);
    }
    drop(check);
    let mut keep = db.as_os_str().to_owned();
    keep.push(".pre-restore");
    let keep = PathBuf::from(keep);
    if db.exists() {
        std::fs::rename(db, &keep)?;
    }
    for ext in ["-wal", "-shm"] {
        let mut p = db.as_os_str().to_owned();
        p.push(ext);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
    std::fs::copy(&info.path, db)?;
    crate::restrict_file(db);
    let conn = Connection::open(db)?;
    let epoch = format!("{:016x}", rand::random::<u64>());
    let n = conn.execute(
        "INSERT INTO meta (key, value) VALUES ('log_epoch', ?1) ON CONFLICT(key) DO UPDATE SET value=?1",
        [epoch.as_str()],
    )?;
    debug_assert_eq!(n, 1);
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mutation, Store};
    use serde_json::json;

    fn put(s: &mut Store, id: &str) {
        let mut m = Mutation::new();
        m.put("note", id, None, &json!({"id": id}));
        m.event("note.put", json!({}), json!({"id": id}));
        s.commit(m).unwrap();
    }

    #[test]
    fn backup_names_round_trip() {
        assert_eq!(
            parse_name("state-v4-1700000000000.db"),
            Some((4, 1700000000000))
        );
        assert_eq!(parse_name("state.db"), None);
        assert_eq!(parse_name("state-vx-1.db"), None);
    }

    #[test]
    fn migration_backs_up_first_and_keeps_three() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("s/state.db");
        // A database at schema version 1 with real content.
        {
            std::fs::create_dir_all(db.parent().unwrap()).unwrap();
            let c = Connection::open(&db).unwrap();
            c.execute_batch(
                "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER);",
            )
            .unwrap();
            c.execute_batch(crate::MIGRATIONS[0]).unwrap();
            c.execute("INSERT INTO schema_migrations VALUES (1, 0)", [])
                .unwrap();
            c.execute(
                "INSERT INTO kv (scope, key, value) VALUES ('t', 'k', 'before')",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&db).unwrap();
        let backups = list_backups(&db);
        assert_eq!(backups.len(), 1);
        assert_eq!(backups[0].schema_version, 1);
        // The backup is the pre-migration schema with the data intact.
        let b = Connection::open(&backups[0].path).unwrap();
        let v: String = b
            .query_row("SELECT value FROM kv WHERE scope='t'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "before");
        let have: i64 = b
            .query_row("SELECT MAX(version) FROM schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(have, 1);
        drop(s);
        // Reopening at the current version takes no new backup.
        let _s = Store::open(&db).unwrap();
        assert_eq!(list_backups(&db).len(), 1);
    }

    #[test]
    fn a_new_database_takes_no_backup() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("s/state.db");
        let _s = Store::open(&db).unwrap();
        assert!(list_backups(&db).is_empty());
    }

    #[test]
    fn only_the_last_three_are_kept() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("s/state.db");
        let s = Store::open(&db).unwrap();
        for v in 1..=5 {
            make_backup(&s.conn, &db, v).unwrap();
        }
        let l = list_backups(&db);
        assert_eq!(l.len(), KEEP_BACKUPS);
        assert_eq!(
            l.iter().map(|b| b.schema_version).collect::<Vec<_>>(),
            vec![5, 4, 3]
        );
    }

    #[test]
    fn restore_rotates_the_log_epoch_and_keeps_the_replaced_db() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("s/state.db");
        let mut s = Store::open(&db).unwrap();
        put(&mut s, "a");
        make_backup(&s.conn, &db, 5).unwrap();
        put(&mut s, "b");
        let epoch_before = s.log_epoch.clone();
        let session = s.session_uuid.clone();
        drop(s);
        let name = list_backups(&db)[0].name.clone();
        restore_backup(&db, &name).unwrap();
        let s = Store::open(&db).unwrap();
        assert_ne!(
            s.log_epoch, epoch_before,
            "restore always rotates log_epoch"
        );
        assert_eq!(s.session_uuid, session);
        let notes: Vec<serde_json::Value> = s.load("note").unwrap();
        assert_eq!(notes.len(), 1, "state is the backup's");
        assert!(d.path().join("s/state.db.pre-restore").exists());
    }

    #[test]
    fn forget_removes_every_backup_and_the_pre_restore_copy() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("state.db");
        let c = Connection::open(&db).unwrap();
        c.execute_batch("CREATE TABLE t (x TEXT); INSERT INTO t VALUES ('secret');")
            .unwrap();
        make_backup(&c, &db, 1).unwrap();
        make_backup(&c, &db, 2).unwrap();
        std::fs::write(d.path().join("state.db.pre-restore"), b"old").unwrap();
        assert_eq!(list_backups(&db).len(), 2);
        assert_eq!(forget_backups(&db), 3);
        assert!(list_backups(&db).is_empty());
        assert!(!d.path().join("state.db.pre-restore").exists());
        // Store connections overwrite deleted content.
        let s = Store::open(&d.path().join("s2/state.db")).unwrap();
        let on: i64 = s
            .conn
            .query_row("PRAGMA secure_delete", [], |r| r.get(0))
            .unwrap();
        assert_eq!(on, 1);
    }

    #[test]
    fn restore_refuses_unknown_and_corrupt_backups() {
        let d = tempfile::tempdir().unwrap();
        let db = d.path().join("s/state.db");
        let s = Store::open(&db).unwrap();
        assert!(restore_backup(&db, "state-v1-1.db").is_err());
        let p = make_backup(&s.conn, &db, 1).unwrap();
        drop(s);
        std::fs::write(&p, b"not a database at all, just text that is long enough").unwrap();
        assert!(restore_backup(&db, p.file_name().unwrap().to_str().unwrap()).is_err());
        assert!(db.exists(), "the live database is untouched");
    }
}
