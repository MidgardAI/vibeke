//! The audit log (09 §11): a separate append-only file `<state>/audit.jsonl` (0600) of
//! security-relevant actions — interaction answers, auto-approvals by policy, policy changes,
//! repo trust grants, elevation requests and grants, token revocations, capability
//! violations (self-answer attempts included) and integration tamper detections — mirrored as
//! `audit.recorded` events in the event log.
//!
//! Entries are hash-chained: each line is a JSON object `{seq, ts, type, actor, subject, data,
//! prev_hash, hash}` where `hash = blake3(prev_hash ‖ the line without its hash field)` and
//! `prev_hash` is the previous entry's hash (64 zeros for the first). The chain head
//! `{seq, hash}` is also kept in `audit.head` next to the log, so truncating the file's tail is
//! detectable too. [`verify_file`] (`audit.verify`, `vibeke doctor`) recomputes the chain.
//! Best-effort: same-UID code can rewrite the whole chain (T9 is out of scope).
//!
//! Free text is redacted with `vk-redact` before it is written; tokens are never passed in.
//!
//! Appends take an exclusive `flock` on `audit.lock` and continue from the file's last entry,
//! so the server and CLI commands that record without a server ([`record_offline`]: plugin
//! and integration installs, remote machine adds) share one chain. Rotation and retention
//! (`crate::audit_retention`) run under the same lock and carry the head hash into the next
//! file.

use crate::Server;
use crate::api::{Ctx, R, invalid, s, u};
use crate::core::Tx;
use serde_json::{Map, Value, json};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::sync::Notify;
use vk_store::now_ms;

/// `prev_hash` of the first entry.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Default)]
pub struct State {
    inner: Mutex<Inner>,
    /// Wakes the flusher when an `audit.recorded` event could not be committed at once.
    wake: Notify,
}

#[derive(Default)]
struct Inner {
    /// `audit.recorded` events not yet committed to the event log: (subject, data).
    pending: Vec<(Value, Value)>,
}

/// The chain head file next to the log.
pub fn head_path(log: &Path) -> PathBuf {
    log.with_extension("head")
}

fn chain_hash(prev: &str, body: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(prev.as_bytes());
    h.update(b"\n");
    h.update(body.as_bytes());
    h.finalize().to_hex().to_string()
}

/// The actor of an API caller, for audit records.
pub fn actor_of(ctx: &Ctx) -> Value {
    let mut a = json!({"kind": if ctx.pane_scope.is_some() { "agent" } else { "user" }, "client": ctx.client_id, "client_kind": ctx.kind});
    if let Some(p) = &ctx.pane_scope {
        a["pane"] = json!(p);
    }
    a
}

/// The last complete line of the log: `(seq, hash)`. Reads the last 64 KiB, then up to 1 MiB.
fn read_tail(log: &Path) -> Option<(u64, String)> {
    let mut f = std::fs::File::open(log).ok()?;
    let len = f.metadata().ok()?.len();
    for window in [64u64 << 10, 1 << 20] {
        let start = len.saturating_sub(window);
        f.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = String::new();
        f.read_to_string(&mut buf).ok()?;
        let mut lines: Vec<&str> = buf.lines().collect();
        if start > 0 && !lines.is_empty() {
            // The first line of a window may be cut.
            lines.remove(0);
        }
        if let Some(line) = lines.iter().rev().find(|l| !l.trim().is_empty())
            && let Ok(v) = serde_json::from_str::<Value>(line)
        {
            return Some((v["seq"].as_u64()?, v["hash"].as_str()?.to_string()));
        }
        if start == 0 {
            return None;
        }
    }
    None
}

/// The last entry of the active log (`(seq, hash)`), if any.
pub(crate) fn tail_of(log: &Path) -> Option<(u64, String)> {
    read_tail(log)
}

/// Exclusive lock on a log's appends (`audit.lock` next to it), released on drop.
pub(crate) struct LogLock(std::fs::File);

impl Drop for LogLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: unlocking our own descriptor.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub(crate) fn lock_log(log: &Path) -> Option<LogLock> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = log.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(log.with_extension("lock"))
        .ok()?;
    loop {
        // SAFETY: flock on a descriptor we own.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Some(LogLock(f));
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
}

fn read_head(log: &Path) -> Option<(u64, String)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(head_path(log)).ok()?).ok()?;
    Some((v["seq"].as_u64()?, v["hash"].as_str()?.to_string()))
}

pub(crate) fn write_head(log: &Path, seq: u64, hash: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let head = head_path(log);
    let tmp = log.with_extension("head.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    f.write_all(json!({"seq": seq, "hash": hash}).to_string().as_bytes())?;
    drop(f);
    std::fs::rename(&tmp, head)
}

pub(crate) fn append_line(log: &Path, line: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(log)?;
    let mut l = line.to_string();
    l.push('\n');
    f.write_all(l.as_bytes())
}

/// Build one chained entry after `(prev_seq, prev_hash)`; returns the line and its hash.
pub(crate) fn entry(
    prev: &(u64, String),
    kind: &str,
    actor: Value,
    subject: Value,
    data: Value,
) -> (String, String, u64) {
    let seq = prev.0 + 1;
    let mut e = Map::new();
    e.insert("seq".into(), json!(seq));
    e.insert("ts".into(), json!(now_ms()));
    e.insert("type".into(), json!(kind));
    e.insert("actor".into(), actor);
    e.insert("subject".into(), subject);
    e.insert("data".into(), data);
    e.insert("prev_hash".into(), json!(prev.1));
    let body = Value::Object(e.clone()).to_string();
    let hash = chain_hash(&prev.1, &body);
    e.insert("hash".into(), json!(hash));
    (Value::Object(e).to_string(), hash, seq)
}

/// Append one entry to `log` under its lock: continue from the file's last entry (recording an
/// `audit.discontinuity` first when the head file says the tail was cut), rotate and prune per
/// `policy`, write the entry and the head. `subject` and `data` must already be redacted.
/// Returns the entry's `(seq, hash)`.
pub(crate) fn append_entry(
    log: &Path,
    kind: &str,
    actor: Value,
    subject: Value,
    data: Value,
    policy: &crate::audit_retention::Policy,
) -> Option<(u64, String)> {
    let _lock = lock_log(log)?;
    let tail = read_tail(log);
    let mut prev = tail.clone().unwrap_or((0, GENESIS.to_string()));
    // The recorded head is ahead of (or differs from) the file: the tail was cut or rewritten.
    // Continue the chain from what is there and say so.
    if let Some(h) = read_head(log)
        && Some(&h) != tail.as_ref()
        && h.0 >= prev.0
    {
        let (line, hash, seq) = entry(
            &prev,
            "audit.discontinuity",
            json!({"kind": "system"}),
            Value::Null,
            json!({"expected_seq": h.0, "expected_hash": h.1, "found_seq": prev.0, "found_hash": prev.1}),
        );
        if append_line(log, &line).is_ok() {
            tracing::warn!(expected = h.0, found = prev.0, "audit log head mismatch");
            let _ = write_head(log, seq, &hash);
            prev = (seq, hash);
        }
    }
    prev = crate::audit_retention::maintain(log, prev, policy);
    let (line, hash, seq) = entry(&prev, kind, actor, subject, data);
    if let Err(e) = append_line(log, &line) {
        tracing::error!(error = %e, log = %log.display(), "audit log append failed");
        return None;
    }
    let _ = write_head(log, seq, &hash);
    Some((seq, hash))
}

/// Append one record to the session's audit log and queue its `audit.recorded` event.
/// Never takes the core lock while the caller might hold it (the event is committed now if
/// the core is free, else by the flusher), so it is safe to call from anywhere. `data` and
/// `subject` are redacted. Returns the entry's sequence number.
pub fn record(
    server: &Server,
    kind: &str,
    actor: Value,
    subject: Value,
    data: Value,
) -> Option<u64> {
    let log = server.paths.audit_log();
    let mut data = data;
    let mut subject = subject;
    vk_redact::redact_json(&mut data);
    vk_redact::redact_json(&mut subject);
    let policy = crate::audit_retention::Policy::current();
    let seq = {
        let mut g = server.security.audit.inner.lock().unwrap();
        let (seq, hash) = append_entry(&log, kind, actor, subject.clone(), data, &policy)?;
        g.pending.push((
            if subject.is_object() {
                subject
            } else {
                json!({})
            },
            json!({"seq": seq, "type": kind, "hash": hash}),
        ));
        seq
    };
    flush(server, false);
    Some(seq)
}

/// Record without a server (CLI commands that change security-relevant state on their own:
/// plugin and integration installs, remote machine adds) into `session`'s audit log. No
/// `audit.recorded` event is emitted; the entry joins the same chain. Best effort: failures
/// are logged, never fatal to the command.
pub fn record_offline(session: &str, kind: &str, subject: Value, data: Value) -> Option<u64> {
    let paths = crate::paths::Paths::new(session);
    if std::fs::create_dir_all(&paths.state).is_err() {
        return None;
    }
    let log = paths.audit_log();
    let mut data = data;
    let mut subject = subject;
    vk_redact::redact_json(&mut data);
    vk_redact::redact_json(&mut subject);
    let policy = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| crate::audit_retention::Policy::from_config(&c))
        .unwrap_or_default();
    let actor = json!({"kind": "user", "client_kind": "cli", "pid": std::process::id()});
    append_entry(&log, kind, actor, subject, data, &policy).map(|(seq, _)| seq)
}

/// Commit queued `audit.recorded` events. With `block`, wait for the core lock; otherwise
/// leave them for the flusher when the core is busy.
pub fn flush(server: &Server, block: bool) {
    let pending = std::mem::take(&mut server.security.audit.inner.lock().unwrap().pending);
    if pending.is_empty() {
        return;
    }
    let guard = if block {
        Some(server.core.lock().unwrap())
    } else {
        server.core.try_lock().ok()
    };
    let Some(mut c) = guard else {
        server
            .security
            .audit
            .inner
            .lock()
            .unwrap()
            .pending
            .splice(0..0, pending);
        server.security.audit.wake.notify_one();
        return;
    };
    let mut tx = Tx::new();
    for (subject, data) in pending {
        tx.event("audit.recorded", subject, data);
    }
    let _ = server.commit(&mut c, tx);
}

/// The flusher: commits events whose record found the core busy.
pub fn start(server: &std::sync::Arc<Server>) {
    crate::audit_retention::start(server);
    let srv = server.clone();
    tokio::spawn(async move {
        loop {
            srv.security.audit.wake.notified().await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let s = srv.clone();
            let _ = tokio::task::spawn_blocking(move || flush(&s, true)).await;
        }
    });
}

/// Audit a rate-limit trip (09 §5.1 rule 7). Called at most once a minute per pane (with the
/// notification), so a runaway caller cannot flood the log.
pub fn rate_limited(server: &Server, pane: &str, handle: &str, method: &str, limit: &str) {
    record(
        server,
        "security.rate_limited",
        json!({"kind": "agent", "pane": pane}),
        json!({"pane": pane, "pane_handle": handle}),
        json!({"method": method, "limit": limit}),
    );
}

/// Audit an OSC 52 clipboard read decision (09 §11 "clipboard decisions"): which pane asked,
/// which client answered, granted or denied, and the size (never the content).
pub fn clipboard_decision(
    server: &Server,
    client: &str,
    pane: &str,
    granted: bool,
    bytes: usize,
    primary: bool,
) {
    record(
        server,
        "clipboard.read_decided",
        json!({"kind": "user", "client": client}),
        json!({"pane": pane}),
        json!({"decision": if granted { "granted" } else { "denied" }, "bytes": bytes, "selection": if primary { "primary" } else { "clipboard" }}),
    );
}

/// Result of [`verify_file`].
#[derive(Debug, Default, Clone)]
pub struct Verify {
    pub exists: bool,
    pub entries: u64,
    pub last_seq: u64,
    pub last_hash: String,
    /// Chain breaks: wrong sequence, wrong `prev_hash`, wrong `hash`, unparsable lines, or a
    /// head file ahead of the log (truncation).
    pub problems: Vec<String>,
    /// Sequence numbers of `audit.discontinuity` entries (a server found the log cut and
    /// continued it).
    pub discontinuities: Vec<u64>,
    pub head: Option<(u64, String)>,
}

impl Verify {
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
    }
    pub fn to_json(&self) -> Value {
        json!({
            "ok": self.ok(),
            "exists": self.exists,
            "entries": self.entries,
            "last_seq": self.last_seq,
            "last_hash": self.last_hash,
            "problems": self.problems,
            "discontinuities": self.discontinuities,
            "head_seq": self.head.as_ref().map(|h| h.0),
        })
    }
}

/// Recompute the hash chain of `log` and compare its end with the head file (09 §11,
/// `vibeke doctor`). Reads the file only; never modifies it.
pub fn verify_file(log: &Path) -> Verify {
    let mut v = Verify {
        last_hash: GENESIS.into(),
        head: read_head(log),
        ..Verify::default()
    };
    let text = match std::fs::read_to_string(log) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(h) = &v.head
                && h.0 > 0
            {
                v.problems.push(format!(
                    "the audit log is missing but its head says {} entries",
                    h.0
                ));
            }
            return v;
        }
        Err(e) => {
            v.problems
                .push(format!("cannot read {}: {e}", log.display()));
            return v;
        }
    };
    v.exists = true;
    let mut hashes: Vec<String> = Vec::new();
    // A rotated or pruned log starts with `audit.rotated`, which carries the previous file's
    // last entry (`prev_hash`, seq - 1): the chain continues from there.
    let mut first_seq = 1u64;
    if let Some(first) = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| serde_json::from_str::<Value>(l).ok())
        && first["type"] == "audit.rotated"
        && let (Some(seq), Some(prev)) = (first["seq"].as_u64(), first["prev_hash"].as_str())
        && seq > 1
    {
        v.last_seq = seq - 1;
        v.last_hash = prev.to_string();
        first_seq = seq;
    }
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let n = i + 1;
        let Ok(Value::Object(mut e)) = serde_json::from_str::<Value>(line) else {
            v.problems.push(format!("line {n}: not a JSON object"));
            continue;
        };
        let seq = e.get("seq").and_then(Value::as_u64).unwrap_or(0);
        let stated = e
            .remove("hash")
            .and_then(|h| h.as_str().map(str::to_string))
            .unwrap_or_default();
        let prev = e
            .get("prev_hash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if seq != v.last_seq + 1 {
            v.problems.push(format!(
                "line {n}: sequence {seq} follows {} (entries missing or reordered)",
                v.last_seq
            ));
        }
        if prev != v.last_hash {
            v.problems.push(format!(
                "line {n}: prev_hash does not match the previous entry"
            ));
        }
        let body = Value::Object(e.clone()).to_string();
        if chain_hash(&prev, &body) != stated {
            v.problems.push(format!(
                "line {n} (seq {seq}): content does not match its hash"
            ));
        }
        if e.get("type").and_then(Value::as_str) == Some("audit.discontinuity") {
            v.discontinuities.push(seq);
        }
        v.entries += 1;
        v.last_seq = seq;
        v.last_hash = stated.clone();
        hashes.push(stated);
        if v.problems.len() > 50 {
            v.problems.push("… (stopped after 50 problems)".into());
            return v;
        }
    }
    if let Some((hs, hh)) = v.head.clone() {
        if hs > v.last_seq {
            v.problems.push(format!(
                "truncated: the head records entry {hs}, the log ends at {}",
                v.last_seq
            ));
        } else if hs >= first_seq
            && hashes
                .get((hs - first_seq) as usize)
                .is_some_and(|h| *h != hh)
        {
            v.problems.push(format!(
                "entry {hs} differs from the recorded head (rewritten)"
            ));
        }
    }
    v
}

/// Entries of `log`, oldest first, filtered: `types` (globs as in `events.read`), `text`
/// (substring of the line), `since_ms`; the last `limit`.
pub fn read_entries(
    log: &Path,
    types: &[String],
    text: Option<&str>,
    since_ms: Option<i64>,
    limit: usize,
) -> Vec<Value> {
    let all = crate::audit_retention::all_text(log);
    let text = text.map(str::to_lowercase);
    let mut out: Vec<Value> = all
        .lines()
        .filter(|l| {
            text.as_ref()
                .is_none_or(|t| l.to_lowercase().contains(t.as_str()))
        })
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| {
            let k = e["type"].as_str().unwrap_or("");
            types.is_empty() || types.iter().any(|g| vk_store::glob_match(g, k))
        })
        .filter(|e| since_ms.is_none_or(|t| e["ts"].as_i64().unwrap_or(0) >= t))
        .collect();
    if out.len() > limit {
        out.drain(..out.len() - limit);
    }
    out
}

fn types_param(p: &Value) -> Vec<String> {
    match p.get("types") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) => s.split(',').map(|s| s.trim().to_string()).collect(),
        _ => vec![],
    }
}

/// `audit.tail | audit.search | audit.verify` (full scope only).
pub fn api(server: &Server, _ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    let log = server.paths.audit_log();
    Some(match method {
        "audit.tail" => {
            let limit = u(p, "limit").unwrap_or(50).clamp(1, 10_000) as usize;
            Ok(
                json!({"entries": read_entries(&log, &types_param(p), None, None, limit), "path": log}),
            )
        }
        "audit.search" => {
            let q = s(p, "query").or_else(|| s(p, "text"));
            let since = p.get("since_ms").and_then(Value::as_i64);
            if q.is_none() && since.is_none() && types_param(p).is_empty() {
                return Some(Err(invalid("audit.search needs query, types or since_ms")));
            }
            let limit = u(p, "limit").unwrap_or(200).clamp(1, 10_000) as usize;
            Ok(
                json!({"entries": read_entries(&log, &types_param(p), q, since, limit), "path": log}),
            )
        }
        "audit.verify" => {
            let mut r = crate::audit_retention::verify_chain(&log).to_json();
            r["path"] = json!(log);
            r["segments"] = json!(crate::audit_retention::segments(&log));
            Ok(r)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_chain(log: &Path, n: u64) {
        let mut prev = (0u64, GENESIS.to_string());
        for i in 0..n {
            let (line, hash, seq) = entry(
                &prev,
                "test.event",
                json!({"kind": "user"}),
                json!({"i": i}),
                json!({"text": format!("entry {i}")}),
            );
            append_line(log, &line).unwrap();
            write_head(log, seq, &hash).unwrap();
            prev = (seq, hash);
        }
    }

    #[test]
    fn chain_verifies_and_detects_tampering_and_truncation() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("audit.jsonl");
        assert!(verify_file(&log).ok(), "no log yet is fine");
        write_chain(&log, 5);
        let v = verify_file(&log);
        assert!(v.ok(), "{:?}", v.problems);
        assert_eq!((v.entries, v.last_seq), (5, 5));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Edit one entry's text: its hash no longer matches.
        let text = std::fs::read_to_string(&log).unwrap();
        let edited = text.replacen("entry 2", "entry X", 1);
        std::fs::write(&log, &edited).unwrap();
        let v = verify_file(&log);
        assert!(!v.ok());
        assert!(
            v.problems.iter().any(|p| p.contains("seq 3")),
            "{:?}",
            v.problems
        );

        // Drop a middle line: sequence and prev_hash break.
        let lines: Vec<&str> = text.lines().collect();
        let mut dropped = lines.clone();
        dropped.remove(1);
        std::fs::write(&log, dropped.join("\n") + "\n").unwrap();
        assert!(!verify_file(&log).ok());

        // Cut the tail: the chain is intact but the head file says more.
        std::fs::write(&log, lines[..3].join("\n") + "\n").unwrap();
        let v = verify_file(&log);
        assert!(
            v.problems.iter().any(|p| p.starts_with("truncated")),
            "{:?}",
            v.problems
        );
    }

    #[test]
    fn filters() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("audit.jsonl");
        write_chain(&log, 4);
        assert_eq!(read_entries(&log, &[], None, None, 2).len(), 2);
        assert_eq!(read_entries(&log, &[], Some("ENTRY 3"), None, 10).len(), 1);
        assert_eq!(
            read_entries(&log, &["test.*".into()], None, None, 10).len(),
            4
        );
        assert!(read_entries(&log, &["other.*".into()], None, None, 10).is_empty());
    }
}
