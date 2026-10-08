//! Pane inbox upkeep (06 A11): the retention sweeper (`paste.inbox_retention`), directory
//! drops (`blob.commit {unpack: "tar"}`), and the `paste.translated` event.

use crate::Server;
use crate::api::{Ctx, R, internal, invalid, resolve_pane, s};
use crate::core::{Tx, subject_pane};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// First sweep shortly after start, then hourly (spec 10 §1.3: one wakeup per hour).
const FIRST_SWEEP: Duration = Duration::from_secs(60);
const SWEEP_EVERY: Duration = Duration::from_secs(3600);

fn retention() -> Duration {
    vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c.paste.inbox_retention.0)
        .unwrap_or(Duration::from_secs(14 * 86400))
}

/// Sweep the inbox now; returns what was removed.
pub fn sweep_now(inbox: &Path, retention: Duration) -> vk_remote::inbox::SweepReport {
    let rep = vk_remote::inbox::sweep(inbox, retention, SystemTime::now());
    if rep.removed > 0 {
        tracing::info!(
            removed = rep.removed,
            bytes = rep.bytes,
            "inbox: removed entries older than paste.inbox_retention"
        );
    }
    rep
}

/// Start the retention sweeper.
pub fn start(server: &Arc<Server>) {
    let _ = server;
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_SWEEP).await;
        loop {
            let inbox = crate::paths::Paths::inbox();
            let ret = retention();
            let _ = tokio::task::spawn_blocking(move || sweep_now(&inbox, ret)).await;
            tokio::time::sleep(SWEEP_EVERY).await;
        }
    });
}

/// `blob.commit {upload_id, unpack: "tar"}`: commit the uploaded tar, unpack it next to itself
/// (`<inbox>/<hash12>/<dirname>`, regular files and directories only) and return the
/// unpacked directory as `path_on_machine`. Re-dropping the same directory reuses the copy
/// already there (content-addressed).
pub fn commit_tar(inbox: &Path, owner: &str, limit: u64, p: &Value) -> R {
    let mut done = crate::api::upload_commit(inbox, owner, p)?;
    let tar = std::path::PathBuf::from(done["path_on_machine"].as_str().unwrap_or_default());
    let dir = tar.parent().map(Path::to_path_buf).unwrap_or_default();
    let stage = dir.join(format!(".unpack-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    let res = std::fs::File::open(&tar)
        .map_err(anyhow::Error::from)
        .and_then(|f| vk_remote::inbox::unpack_tar(std::io::BufReader::new(f), &stage, limit));
    let _ = std::fs::remove_file(&tar);
    let tops = match res {
        Ok(t) => t,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(invalid(format!("directory drop: {e:#}")));
        }
    };
    let [top] = tops.as_slice() else {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(invalid(
            "directory drop: the archive must hold exactly one top-level directory",
        ));
    };
    let target = dir.join(top);
    if std::fs::symlink_metadata(&target).is_err() {
        std::fs::rename(stage.join(top), &target).map_err(internal)?;
    }
    let _ = std::fs::remove_dir_all(&stage);
    done["path_on_machine"] = json!(target);
    done["path"] = json!(target);
    done["unpacked"] = json!(true);
    Ok(done)
}

/// Basename only: paths of the user's machine never enter the event log (09 §9).
fn basename(n: &str) -> String {
    n.rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .chars()
        .take(255)
        .collect()
}

/// `paste.translated {pane, files: [{blob, bytes, local_name}], target_namespace}` (06 A11.3):
/// the client reports a paste it rewrote after uploading. Recorded as the `paste.translated`
/// event with basenames only.
pub fn paste_translated(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let pane = resolve_pane(server, ctx, s(p, "pane"))?;
    let files = p
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("missing param `files`"))?;
    if files.is_empty() || files.len() > 256 {
        return Err(invalid("`files` must list 1..=256 files"));
    }
    let mut out = Vec::new();
    for f in files {
        let blob = s(f, "blob").unwrap_or("");
        if blob.len() > 64 || !blob.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(invalid("files[].blob must be a hex hash"));
        }
        out.push(json!({
            "blob": blob,
            "bytes": f.get("bytes").and_then(Value::as_u64).unwrap_or(0),
            "local_name": basename(s(f, "local_name").unwrap_or("")),
            "dir": f.get("dir").and_then(Value::as_bool).unwrap_or(false),
        }));
    }
    let ns = s(p, "target_namespace").unwrap_or("ssh");
    if ns.len() > 128 || ns.chars().any(char::is_control) {
        return Err(invalid("invalid `target_namespace`"));
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(
        "paste.translated",
        subject_pane(&pane),
        json!({"files": out, "target_namespace": ns}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(json!({"recorded": out.len()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_drop_unpacks_once_and_reuses() {
        let src = tempfile::tempdir().unwrap();
        let d = src.path().join("site");
        std::fs::create_dir_all(d.join("css")).unwrap();
        std::fs::write(d.join("index.html"), b"<h1>hi</h1>").unwrap();
        std::fs::write(d.join("css/a.css"), b"h1{}").unwrap();
        let mut tar = Vec::new();
        vk_remote::inbox::pack_dir(&d, &mut tar, 1 << 20).unwrap();
        let inbox = tempfile::tempdir().unwrap();
        let up = |owner: &str| {
            use base64::Engine;
            let id = crate::api::upload_begin(
                inbox.path(),
                owner,
                1 << 20,
                &json!({"name": "site.tar", "size": tar.len()}),
            )
            .unwrap()["upload_id"]
                .as_str()
                .unwrap()
                .to_string();
            crate::api::upload_append(
                owner,
                &json!({"upload_id": id, "offset": 0,
                        "data_b64": base64::engine::general_purpose::STANDARD.encode(&tar)}),
            )
            .unwrap();
            commit_tar(inbox.path(), owner, 1 << 20, &json!({"upload_id": id})).unwrap()
        };
        let r = up("c1");
        let path = std::path::PathBuf::from(r["path_on_machine"].as_str().unwrap());
        assert!(path.ends_with("site"), "{path:?}");
        assert!(path.starts_with(inbox.path()));
        assert_eq!(
            std::fs::read(path.join("index.html")).unwrap(),
            b"<h1>hi</h1>"
        );
        assert_eq!(r["unpacked"], true);
        // The tar itself is gone; the same drop again lands on the same directory.
        assert!(!path.parent().unwrap().join("site.tar").exists());
        let r2 = up("c2");
        assert_eq!(r2["path_on_machine"], r["path_on_machine"]);
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["site".to_string()]);
    }

    #[test]
    fn basenames_only() {
        assert_eq!(basename("/Users/alice/Desktop/Shot 1.png"), "Shot 1.png");
        assert_eq!(basename("C:\\x\\y.txt"), "y.txt");
        assert_eq!(basename("plain"), "plain");
    }
}
