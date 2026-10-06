//! Audit log retention and rotation (09 §9.3 `retention.audit`, §11), with the hash chain kept
//! across files.
//!
//! The active log is `<state>/audit.jsonl`. When it grows past `max_bytes`, or its oldest entry
//! is older than `retention_days`, it is renamed to a segment `audit-<first>-<last>.jsonl` (the
//! sequence numbers it holds, zero-padded so names sort in order) and the new active file
//! starts with an `audit.rotated {segment, first_seq, last_seq}` entry whose `prev_hash` is the
//! segment's last hash: the head hash is carried into the new file, so the chain runs
//! unbroken from segment to segment. Segments whose newest entry (the file's mtime) is older
//! than `retention_days`, and the oldest ones beyond `max_segments`, are deleted; each deletion
//! is recorded as an `audit.pruned {segments, through_seq}` entry. The first remaining file
//! then starts with an `audit.rotated` entry, which [`verify_chain`] (and
//! `audit::verify_file`) accept as a chain start: removing entries anywhere else still breaks
//! the chain.
//!
//! Configuration: `[security.audit] max_bytes` (bytes or a size such as `"16MiB"`; default
//! 16 MiB), `retention_days` (default 90), `max_segments` (default 20). Maintenance runs under
//! the log's `flock` on every append (from the server or a CLI writing an offline record) and
//! hourly in the server.

use crate::audit::{GENESIS, Verify, append_line, entry, verify_file};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Retention settings (`[security.audit]`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    pub max_bytes: u64,
    pub max_age: Duration,
    pub max_segments: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            max_bytes: 16 * 1024 * 1024,
            max_age: Duration::from_secs(90 * 86_400),
            max_segments: 20,
        }
    }
}

impl Policy {
    pub fn from_config(cfg: &vk_config::Config) -> Self {
        let mut p = Policy::default();
        let Some(a) = cfg
            .extra
            .get("security")
            .and_then(|s| s.get("audit"))
            .and_then(toml::Value::as_table)
        else {
            return p;
        };
        match a.get("max_bytes") {
            Some(toml::Value::Integer(n)) if *n > 0 => p.max_bytes = *n as u64,
            Some(toml::Value::String(v)) => {
                if let Ok(b) = vk_config::ByteSize::parse(v) {
                    p.max_bytes = b.0.max(1);
                }
            }
            _ => {}
        }
        if let Some(d) = a.get("retention_days").and_then(toml::Value::as_integer)
            && d > 0
        {
            p.max_age = Duration::from_secs(d as u64 * 86_400);
        }
        if let Some(n) = a.get("max_segments").and_then(toml::Value::as_integer)
            && n > 0
        {
            p.max_segments = n as usize;
        }
        p
    }

    /// The policy of the config the server applies now.
    pub fn current() -> Self {
        Self::from_config(&crate::config_api::current())
    }
}

/// Rotated segments of `log`, oldest first.
pub fn segments(log: &Path) -> Vec<PathBuf> {
    let Some(dir) = log.parent() else {
        return vec![];
    };
    let stem = log
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("audit")
        .to_string();
    let prefix = format!("{stem}-");
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".jsonl"))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn first_entry(log: &Path) -> Option<Value> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(log).ok()?;
    let mut line = String::new();
    BufReader::new(f).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim()).ok()
}

fn now_ms() -> i64 {
    vk_store::now_ms()
}

/// Rotate and prune as the policy says, writing the marker entries. Called with the log's
/// lock held; `prev` is the last `(seq, hash)` of the active log. Returns the new last entry.
pub fn maintain(log: &Path, prev: (u64, String), policy: &Policy) -> (u64, String) {
    let mut prev = prev;
    let size = std::fs::metadata(log).map(|m| m.len()).unwrap_or(0);
    let first = first_entry(log);
    let first_seq = first
        .as_ref()
        .and_then(|e| e["seq"].as_u64())
        .unwrap_or(prev.0.max(1));
    let too_old = first
        .as_ref()
        .and_then(|e| e["ts"].as_i64())
        .is_some_and(|ts| now_ms() - ts > policy.max_age.as_millis() as i64);
    // Never rotate a file that holds only its own rotation marker.
    let only_marker = first
        .as_ref()
        .is_some_and(|e| e["type"] == "audit.rotated" && e["seq"].as_u64() == Some(prev.0));
    if size > 0 && prev.0 >= first_seq && !only_marker && (size >= policy.max_bytes || too_old) {
        let name = format!(
            "{}-{first_seq:012}-{:012}.jsonl",
            log.file_stem().and_then(|s| s.to_str()).unwrap_or("audit"),
            prev.0
        );
        let seg = log.with_file_name(&name);
        if std::fs::rename(log, &seg).is_ok() {
            let (line, hash, seq) = entry(
                &prev,
                "audit.rotated",
                json!({"kind": "system"}),
                Value::Null,
                json!({"segment": name, "first_seq": first_seq, "last_seq": prev.0, "reason": if too_old { "age" } else { "size" }}),
            );
            if append_line(log, &line).is_ok() {
                prev = (seq, hash);
            }
        }
    }
    let pruned = prune(log, policy);
    if !pruned.is_empty() {
        let through = pruned
            .iter()
            .filter_map(|p| seg_last_seq(p))
            .max()
            .unwrap_or(0);
        let names: Vec<String> = pruned
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
            .collect();
        let (line, hash, seq) = entry(
            &prev,
            "audit.pruned",
            json!({"kind": "system"}),
            Value::Null,
            json!({"segments": names, "through_seq": through}),
        );
        if append_line(log, &line).is_ok() {
            prev = (seq, hash);
        }
    }
    prev
}

fn seg_last_seq(p: &Path) -> Option<u64> {
    let n = p.file_stem()?.to_str()?;
    n.rsplit('-').next()?.parse().ok()
}

/// Delete segments older than the retention or beyond `max_segments`; returns them.
pub fn prune(log: &Path, policy: &Policy) -> Vec<PathBuf> {
    let segs = segments(log);
    let now = SystemTime::now();
    let excess = segs.len().saturating_sub(policy.max_segments);
    let mut removed = vec![];
    for (i, s) in segs.iter().enumerate() {
        let old = std::fs::metadata(s)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > policy.max_age);
        if (i < excess || old) && std::fs::remove_file(s).is_ok() {
            removed.push(s.clone());
        }
    }
    removed
}

/// Every retained entry's line, oldest first (segments, then the active log).
pub fn all_text(log: &Path) -> String {
    let mut out = String::new();
    for f in segments(log).iter().map(PathBuf::as_path).chain([log]) {
        if let Ok(t) = std::fs::read_to_string(f) {
            out.push_str(&t);
            if !t.ends_with('\n') && !t.is_empty() {
                out.push('\n');
            }
        }
    }
    out
}

/// Verify the whole retained chain: each segment, then the active log, each file continuing
/// the previous one (`audit.rotated` carries the previous file's last hash). The result's
/// counts cover all files; `segments` is added to its JSON by `audit.verify`.
pub fn verify_chain(log: &Path) -> Verify {
    let segs = segments(log);
    let mut total = Verify {
        last_hash: GENESIS.into(),
        ..Verify::default()
    };
    let mut carried: Option<(u64, String)> = None;
    for f in segs.iter().map(PathBuf::as_path).chain([log]) {
        let is_active = f == log;
        let mut v = verify_file(f);
        if !is_active {
            // A segment's head check is meaningless (the head file follows the active log).
            v.problems
                .retain(|p| !p.starts_with("truncated") && !p.contains("recorded head"));
        }
        if let (Some((cs, ch)), Some(first)) = (&carried, first_entry(f))
            && (first["seq"].as_u64() != Some(cs + 1) || first["prev_hash"].as_str() != Some(ch))
        {
            v.problems.push(format!(
                "{}: does not continue the previous file (entries {}.. missing)",
                f.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                cs + 1
            ));
        }
        if v.exists {
            carried = Some((v.last_seq, v.last_hash.clone()));
            total.exists = true;
            total.entries += v.entries;
            total.last_seq = v.last_seq;
            total.last_hash = v.last_hash.clone();
        }
        total.discontinuities.extend(v.discontinuities);
        total.problems.extend(v.problems);
        if is_active {
            total.head = v.head;
        }
    }
    total
}

/// Run maintenance now (hourly in the server), under the log's lock.
pub fn sweep(log: &Path, policy: &Policy) {
    let Some(_lock) = crate::audit::lock_log(log) else {
        return;
    };
    let Some(prev) = crate::audit::tail_of(log) else {
        // No active log: only old segments can be pruned (nothing to chain a marker to).
        let _ = prune(log, policy);
        return;
    };
    let new = maintain(log, prev.clone(), policy);
    if new != prev {
        let _ = crate::audit::write_head(log, new.0, &new.1);
    }
}

/// Hourly maintenance for the server's audit log.
pub fn start(server: &std::sync::Arc<crate::Server>) {
    let log = server.paths.audit_log();
    tokio::spawn(async move {
        loop {
            let l = log.clone();
            let _ = tokio::task::spawn_blocking(move || sweep(&l, &Policy::current())).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(log: &Path, n: u64, policy: &Policy) {
        for i in 0..n {
            crate::audit::append_entry(
                log,
                "test.event",
                json!({"kind": "user"}),
                json!({"i": i}),
                json!({"text": format!("entry {i} {}", "x".repeat(100))}),
                policy,
            )
            .unwrap();
        }
    }

    #[test]
    fn rotation_keeps_the_chain_and_pruning_is_recorded() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("audit.jsonl");
        let policy = Policy {
            max_bytes: 1000,
            max_age: Duration::from_secs(3600),
            max_segments: 3,
        };
        write(&log, 40, &policy);
        let segs = segments(&log);
        assert!(!segs.is_empty() && segs.len() <= 3, "{segs:?}");
        let v = verify_chain(&log);
        assert!(v.ok(), "{:?}", v.problems);
        let text = all_text(&log);
        assert!(text.contains("\"audit.rotated\""));
        assert!(text.contains("\"audit.pruned\""), "beyond max_segments");
        // The active file starts with the carried head.
        let first = first_entry(&log).unwrap();
        assert_eq!(first["type"], "audit.rotated");
        assert!(verify_file(&log).ok(), "{:?}", verify_file(&log).problems);
        // Removing a middle segment breaks the chain.
        let segs = segments(&log);
        if segs.len() >= 2 {
            std::fs::remove_file(&segs[segs.len() - 1]).unwrap();
            assert!(!verify_chain(&log).ok());
        }
    }

    #[test]
    fn old_segments_are_pruned_by_age() {
        let d = tempfile::tempdir().unwrap();
        let log = d.path().join("audit.jsonl");
        let p = Policy {
            max_bytes: 500,
            ..Policy::default()
        };
        write(&log, 10, &p);
        let segs = segments(&log);
        assert!(!segs.is_empty());
        let zero = Policy {
            max_age: Duration::from_secs(0),
            ..p
        };
        std::thread::sleep(Duration::from_millis(20));
        sweep(&log, &zero);
        assert!(segments(&log).is_empty(), "all segments past retention");
        assert!(all_text(&log).contains("\"audit.pruned\""));
        assert!(verify_chain(&log).ok(), "{:?}", verify_chain(&log).problems);
    }

    #[test]
    fn policy_from_config() {
        let (cfg, _) = vk_config::Config::parse(
            "[security.audit]\nmax_bytes = \"1MiB\"\nretention_days = 7\nmax_segments = 4\n",
            Path::new("c.toml"),
        )
        .unwrap();
        let p = Policy::from_config(&cfg);
        assert_eq!(p.max_bytes, 1024 * 1024);
        assert_eq!(p.max_age, Duration::from_secs(7 * 86_400));
        assert_eq!(p.max_segments, 4);
    }
}
