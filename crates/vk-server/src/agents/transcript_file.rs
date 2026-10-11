//! Safe access to harness transcript files (04 §10).
//!
//! A transcript path comes from the agent itself (hook payloads, self-report, a headless
//! harness's stream), so the server treats it as untrusted:
//!
//! - **Accepting a path** ([`accept`]): it must be absolute. A contained pane (sandbox,
//!   container, VM, cloud) may only name a file inside the roots its processes can see
//!   ([`vk_proto::model::Isolation::visible_roots`]: the checkout and the pane's private dir,
//!   which holds the relocated harness home). A sandboxed agent then cannot point the server at
//!   a host transcript and read it back through `agent.read` or `agent.transcript`. Host panes
//!   can read every file of the user already, so their paths are accepted as reported: the
//!   user's own `CLAUDE_CONFIG_DIR`, `CODEX_HOME` or `PI_CODING_AGENT_DIR` keep working.
//! - **Reading a file** ([`open_regular`], [`read_tail`], [`read_from`]): the file is opened
//!   with `O_NONBLOCK | O_NOFOLLOW` and must be a regular file (no FIFO, socket or device, so a
//!   `/dev/zero` or a FIFO cannot exhaust memory or hang a worker), and only a bounded tail is
//!   read. Callers run these on the blocking pool.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use crate::Server;

/// Longest path accepted from a payload.
const MAX_PATH: usize = 4096;

/// Open `path` read-only without following a final symlink and without blocking on a FIFO;
/// refuse anything but a regular file.
pub fn open_regular(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    if !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "transcript path is not absolute",
        ));
    }
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    // fstat on the opened descriptor: no race between the check and the read.
    let meta = f.metadata()?;
    if !meta.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "transcript is not a regular file",
        ));
    }
    Ok(f)
}

/// The last `max_bytes` of a regular file, decoded lossily. `true` when the read started in
/// the middle of the file (the first line may be partial).
pub fn read_tail(path: &Path, max_bytes: u64) -> Option<(String, bool)> {
    let mut f = open_regular(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    // `take` bounds the read even when the file grows while it is read.
    f.take(max_bytes).read_to_end(&mut buf).ok()?;
    Some((String::from_utf8_lossy(&buf).into_owned(), start > 0))
}

/// Up to `max` bytes from `offset` (from 0 when the file shrank below `offset`): the bytes and
/// the offset they start at.
pub fn read_from(path: &Path, offset: u64, max: u64) -> std::io::Result<(Vec<u8>, u64)> {
    let mut f = open_regular(path)?;
    let len = f.metadata()?.len();
    // Truncated or replaced: start over.
    let start = if len < offset { 0 } else { offset };
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf)?;
    Ok((buf, start))
}

/// The transcript path an agent of `pane` reported, if the server may use it (see the module
/// docs). The returned path is normalized; for a contained pane it is resolved through
/// symlinks.
pub fn accept(server: &Server, pane: &str, raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > MAX_PATH || raw.contains('\0') {
        return None;
    }
    let path = Path::new(raw);
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    // A closed pane keeps its stored record: an ended run's transcript stays readable.
    let isolation = server.with_core(|c| {
        c.pane(pane).map(|p| p.isolation.clone()).or_else(|| {
            c.store
                .find::<vk_proto::model::Pane>("pane", pane)
                .ok()
                .flatten()
                .map(|p| p.isolation)
        })
    })?;
    if !isolation.is_contained() {
        return Some(raw.to_string());
    }
    let resolved = resolve_existing_prefix(path)?;
    let inside = isolation.visible_roots.iter().any(|root| {
        std::fs::canonicalize(root)
            .ok()
            .is_some_and(|r| resolved.starts_with(&r))
    });
    inside.then(|| resolved.to_string_lossy().into_owned())
}

/// `path` with its longest existing prefix resolved through symlinks and the rest appended
/// as is (a harness creates its transcript, and sometimes its directory, at the first prompt).
fn resolve_existing_prefix(path: &Path) -> Option<PathBuf> {
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur = path;
    loop {
        match std::fs::canonicalize(cur) {
            Ok(mut base) => {
                for part in rest.iter().rev() {
                    base.push(part);
                }
                return Some(base);
            }
            Err(_) => {
                rest.push(cur.file_name()?);
                cur = cur.parent()?;
            }
        }
    }
}

/// Drop a payload's `transcript_path` the server may not use for `pane` (see [`accept`]), and
/// replace an accepted one with its normalized form.
pub fn sanitize_payload(server: &Server, pane: &str, payload: &mut serde_json::Value) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };
    for key in ["transcript_path", "session_file", "sessionFile"] {
        let Some(v) = obj.get(key) else {
            continue;
        };
        let accepted = v.as_str().and_then(|raw| accept(server, pane, raw));
        match accepted {
            Some(p) => {
                obj.insert(key.to_string(), serde_json::Value::String(p));
            }
            None if v.is_null() => {}
            None => {
                tracing::debug!(pane, key, "transcript path refused");
                obj.remove(key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_regular_refuses_fifos_devices_symlinks_and_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("t.jsonl");
        std::fs::write(&file, "{\"a\":1}\n").unwrap();
        assert!(open_regular(&file).is_ok());

        // A FIFO with no writer must not block the open (O_NONBLOCK) and is refused.
        let fifo = dir.path().join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: valid C string, creating a FIFO in our own temp dir.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(open_regular(&fifo).is_err());

        assert!(open_regular(Path::new("/dev/zero")).is_err());

        let link = dir.path().join("link.jsonl");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(open_regular(&link).is_err(), "a final symlink is refused");

        assert!(open_regular(Path::new("t.jsonl")).is_err());
    }

    #[test]
    fn read_tail_reads_only_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("t.jsonl");
        let body: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&file, &body).unwrap();
        let (tail, mid) = read_tail(&file, 64).unwrap();
        assert!(mid);
        assert!(tail.len() <= 64);
        assert!(tail.ends_with("line 999\n"));
        let (all, mid) = read_tail(&file, 1 << 20).unwrap();
        assert!(!mid);
        assert_eq!(all, body);
        assert!(read_tail(Path::new("/dev/zero"), 64).is_none());
    }

    #[test]
    fn read_from_restarts_after_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("t.jsonl");
        std::fs::write(&file, "abcdef").unwrap();
        let (b, start) = read_from(&file, 2, 3).unwrap();
        assert_eq!((b.as_slice(), start), (&b"cde"[..], 2));
        let (b, start) = read_from(&file, 100, 3).unwrap();
        assert_eq!((b.as_slice(), start), (&b"abc"[..], 0));
    }

    #[test]
    fn resolve_existing_prefix_resolves_symlinked_parents() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let got = resolve_existing_prefix(&alias.join("new/t.jsonl")).unwrap();
        assert_eq!(
            got,
            std::fs::canonicalize(&real).unwrap().join("new/t.jsonl")
        );
    }

    #[test]
    fn contained_panes_may_only_name_transcripts_inside_their_visible_roots() {
        use crate::core::Tx;
        use crate::hardening::testkit::{sample_pane, server};
        use vk_proto::model::{Isolation, IsolationLevel};
        let dir = tempfile::Builder::new()
            .prefix("vktf")
            .tempdir_in("/tmp")
            .unwrap();
        let srv = server(dir.path(), "tf");
        let root = dir.path().join("box");
        std::fs::create_dir_all(root.join("home/claude/projects")).unwrap();
        let host = dir.path().join("host.jsonl");
        std::fs::write(&host, "{}\n").unwrap();
        {
            let mut c = srv.core.lock().unwrap();
            let mut tx = Tx::new();
            tx.pane(sample_pane("host-pane", "w"));
            let mut boxed = sample_pane("box-pane", "w");
            boxed.isolation = Isolation {
                level: IsolationLevel::Sandbox,
                provider: "seatbelt".into(),
                visible_roots: vec![root.to_string_lossy().into_owned()],
                ..Isolation::default()
            };
            tx.pane(boxed);
            srv.commit(&mut c, tx).unwrap();
        }
        let host_s = host.to_string_lossy().into_owned();
        // Host panes: any absolute path, as reported.
        assert_eq!(accept(&srv, "host-pane", &host_s), Some(host_s.clone()));
        assert_eq!(accept(&srv, "host-pane", "relative.jsonl"), None);
        assert_eq!(accept(&srv, "host-pane", "/a/../b.jsonl"), None);
        // Contained panes: only inside the box, also for a file that does not exist yet.
        assert_eq!(accept(&srv, "box-pane", &host_s), None);
        let inside = root.join("home/claude/projects/new/s.jsonl");
        let got = accept(&srv, "box-pane", &inside.to_string_lossy()).unwrap();
        assert!(got.ends_with("home/claude/projects/new/s.jsonl"));
        // A symlink inside the box that points out of it is resolved and refused.
        let link = root.join("home/escape");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        assert_eq!(
            accept(&srv, "box-pane", &link.join("host.jsonl").to_string_lossy()),
            None
        );
        // Unknown panes: refused.
        assert_eq!(accept(&srv, "nope", &host_s), None);

        let mut payload = serde_json::json!({"transcript_path": host_s, "session_id": "s"});
        sanitize_payload(&srv, "box-pane", &mut payload);
        assert!(payload.get("transcript_path").is_none());
        assert_eq!(payload["session_id"], "s");
    }
}
