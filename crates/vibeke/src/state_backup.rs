//! `vibeke doctor --list-backups` and `--restore-backup NAME` (02 §3): the pre-migration copies
//! of `state.db` (`<state>/backups/`, last three) and the offline restore. Restoring replaces the
//! session's database, keeps the replaced one as `state.db.pre-restore`, and always rotates
//! `log_epoch` so no client cursor from before the restore is ever interpreted against it. It
//! refuses while the session's server is running, and holds the state lock for the whole swap.

use serde_json::json;
use std::io::IsTerminal;
use vk_cli::{EXIT_OK, EXIT_USAGE, Global};
use vk_server::paths::Paths;
use vk_store::backup;

fn want_json(g: &Global) -> bool {
    g.json == Some(true) || !std::io::stdout().is_terminal()
}

fn when(ms: i64) -> String {
    let secs = ms / 1000;
    format!("{secs} (unix)")
}

pub async fn run(g: &Global, args: &[String]) -> i32 {
    let mut list = false;
    let mut restore: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list-backups" => list = true,
            "--restore-backup" => match it.next() {
                Some(n) if !n.starts_with("--") => restore = Some(n.clone()),
                _ => {
                    eprintln!("vibeke doctor --restore-backup NAME  (see --list-backups)");
                    return EXIT_USAGE;
                }
            },
            "--json" => {}
            other => {
                eprintln!(
                    "vibeke doctor --list-backups | --restore-backup NAME [--json]  (unexpected `{other}`)"
                );
                return EXIT_USAGE;
            }
        }
    }
    if list && restore.is_some() {
        eprintln!("vibeke doctor: --list-backups and --restore-backup are separate");
        return EXIT_USAGE;
    }
    let p = Paths::new(&g.session);
    let db = p.db();
    if let Some(name) = restore {
        return restore_one(g, &p, &db, &name).await;
    }
    let backups = backup::list_backups(&db);
    if want_json(g) {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "session": g.session,
                "dir": backup::backup_dir(&db),
                "keep": backup::KEEP_BACKUPS,
                "backups": backups,
            }))
            .unwrap_or_default()
        );
    } else if backups.is_empty() {
        println!(
            "session `{}`: no backups in {} (one is taken before a schema migration; the last {} are kept)",
            g.session,
            backup::backup_dir(&db).display(),
            backup::KEEP_BACKUPS
        );
    } else {
        println!(
            "session `{}`: backups in {} (taken before a schema migration)",
            g.session,
            backup::backup_dir(&db).display()
        );
        for b in &backups {
            println!(
                "  {}  schema v{}  {} bytes  at {}",
                b.name,
                b.schema_version,
                b.bytes,
                when(b.created_at_ms)
            );
        }
    }
    EXIT_OK
}

async fn restore_one(g: &Global, p: &Paths, db: &std::path::Path, name: &str) -> i32 {
    if crate::doctor::session_running(g).await {
        eprintln!(
            "refusing to restore a backup of session `{}` while its server is running; run `vibeke server stop` first (panes survive), then retry",
            g.session
        );
        return 1;
    }
    let _lock = match p.try_lock_state() {
        Ok(Some(l)) => l,
        Ok(None) => {
            eprintln!(
                "refusing to restore a backup of session `{}`: its state is locked ({}), so its server is running (or starting) or another doctor run is in progress",
                g.session,
                p.state_lock().display()
            );
            return 1;
        }
        Err(e) => {
            eprintln!("cannot lock {}: {e}", p.state_lock().display());
            return 1;
        }
    };
    match backup::restore_backup(db, name) {
        Ok(info) => {
            if want_json(g) {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "session": g.session,
                        "restored": info,
                        "replaced": format!("{}.pre-restore", db.display()),
                        "log_epoch_rotated": true,
                    }))
                    .unwrap_or_default()
                );
            } else {
                println!(
                    "session `{}`: restored {} (schema v{}); the database it replaced is kept as {}.pre-restore; log_epoch rotated, so clients resubscribe from a snapshot. The next server start migrates it forward again.",
                    g.session,
                    info.name,
                    info.schema_version,
                    db.display()
                );
            }
            EXIT_OK
        }
        Err(e) => {
            eprintln!("restore failed (the current database is untouched): {e:#}");
            1
        }
    }
}
