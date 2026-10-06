//! Result cache with scope-bound keys (14 §8).
//!
//! A cache hit answers a request without contacting the provider. The key binds everything
//! that could make a stored result wrong or too broad to reuse:
//!
//! - the **requester's access scope** (who is asking and what they may read),
//! - the **workspace grants** in force (a digest of the grant records of every workspace the
//!   request touches: re-granting, narrowing or revoking changes it, so a changed or revoked
//!   grant can never hit),
//! - the **source content digests** (a changed source misses),
//! - the **feature, prompt and schema versions**, and
//! - the **resolved profile** (connection, endpoint fingerprint, model, limits).
//!
//! Consent is still checked on IDs before the lookup; the cache only skips the provider call.
//! Entries expire with the retention window and are removed by `forget` of their workspace.
//! The cache holds validated drafts and metadata, never prompts.

use crate::consent::Grant;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Entries kept (oldest dropped first).
pub const MAX_ENTRIES: usize = 128;

pub struct KeyParts<'a> {
    /// The requester's access scope (for example `full`).
    pub access_scope: &'a str,
    /// Digest of the grants in force for the request's workspaces ([`grants_stamp`]).
    pub grants_stamp: &'a str,
    /// Digests of the included sources, in order.
    pub source_digests: &'a [String],
    pub feature: &'a str,
    pub prompt_version: &'a str,
    pub schema_version: &'a str,
    /// Connection id, endpoint fingerprint, model and limits of the resolved profile.
    pub profile: &'a str,
}

pub fn key(p: &KeyParts<'_>) -> String {
    let mut h = blake3::Hasher::new();
    for part in [
        p.access_scope,
        p.grants_stamp,
        p.feature,
        p.prompt_version,
        p.schema_version,
        p.profile,
    ] {
        h.update(part.as_bytes());
        h.update(&[0]);
    }
    for d in p.source_digests {
        h.update(d.as_bytes());
        h.update(&[1]);
    }
    h.finalize().to_hex()[..32].to_string()
}

/// Digest of the grant records that authorize a request over `workspaces` through
/// `connection`. Any change to a record (classes, operations, endpoint fingerprint, grant
/// time) or its removal changes the stamp.
pub fn grants_stamp(grants: &[Grant], workspaces: &[String], connection: &str) -> String {
    let mut h = blake3::Hasher::new();
    let mut ws: Vec<&String> = workspaces.iter().collect();
    ws.sort();
    for w in ws {
        h.update(w.as_bytes());
        match grants
            .iter()
            .find(|g| g.workspace == *w && g.connection == connection)
        {
            Some(g) => {
                let mut classes = g.classes.clone();
                classes.sort();
                let mut ops = g.operations.clone();
                ops.sort();
                h.update(
                    format!(
                        "|{}|{}|{}|{}|{}",
                        g.fingerprint,
                        classes.join(","),
                        ops.join(","),
                        g.granted_at_ms,
                        g.granted_by
                    )
                    .as_bytes(),
                );
            }
            None => {
                h.update(b"|none");
            }
        }
        h.update(&[2]);
    }
    h.finalize().to_hex()[..16].to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub operation: String,
    /// Canonical workspace paths the content came from (`forget` removes by these).
    pub workspaces: Vec<String>,
    pub created_ms: i64,
    pub expires_ms: i64,
    /// The validated draft and the metadata needed to present it (never prompts).
    pub data: Value,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cache {
    pub entries: Vec<Entry>,
}

impl Cache {
    pub fn get(&self, key: &str, now_ms: i64) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.key == key && e.expires_ms > now_ms)
    }

    pub fn put(&mut self, e: Entry) {
        self.entries.retain(|x| x.key != e.key);
        self.entries.push(e);
        while self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
    }

    /// Drop expired entries; returns how many went.
    pub fn sweep(&mut self, now_ms: i64) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| e.expires_ms > now_ms);
        before - self.entries.len()
    }

    /// Remove every entry derived from `workspace` (a canonical path). Returns the count.
    pub fn purge_workspace(&mut self, workspace: &str) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|e| !e.workspaces.iter().any(|w| w == workspace));
        before - self.entries.len()
    }

    pub fn purge_all(&mut self) -> usize {
        let n = self.entries.len();
        self.entries.clear();
        n
    }

    /// Remove entries created before `ms` (`forget --before`).
    pub fn purge_before(&mut self, ms: i64) -> usize {
        let before = self.entries.len();
        self.entries.retain(|e| e.created_ms >= ms);
        before - self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn grant(ws: &str, at: i64, classes: &[&str]) -> Grant {
        Grant {
            workspace: ws.into(),
            connection: "c".into(),
            fingerprint: "fp".into(),
            adapter: "anthropic".into(),
            endpoint_host: "h".into(),
            operations: vec![],
            classes: classes.iter().map(|s| s.to_string()).collect(),
            auto_send: vec![],
            granted_at_ms: at,
            granted_by: "u".into(),
        }
    }

    fn parts<'a>(stamp: &'a str, digests: &'a [String]) -> KeyParts<'a> {
        KeyParts {
            access_scope: "full",
            grants_stamp: stamp,
            source_digests: digests,
            feature: "briefing",
            prompt_version: "v1",
            schema_version: "s1",
            profile: "c|fp|model|12000",
        }
    }

    #[test]
    fn every_key_part_changes_the_key() {
        let d = vec!["a".to_string(), "b".to_string()];
        let base = key(&parts("g1", &d));
        assert_eq!(base, key(&parts("g1", &d)));
        let d2 = vec!["a".to_string(), "c".to_string()];
        assert_ne!(base, key(&parts("g1", &d2)), "source content");
        assert_ne!(base, key(&parts("g2", &d)), "grants");
        let mut p = parts("g1", &d);
        p.access_scope = "other";
        assert_ne!(base, key(&p), "access scope");
        let mut p = parts("g1", &d);
        p.feature = "handoff";
        assert_ne!(base, key(&p), "feature");
        let mut p = parts("g1", &d);
        p.prompt_version = "v2";
        assert_ne!(base, key(&p), "prompt version");
        let mut p = parts("g1", &d);
        p.schema_version = "s2";
        assert_ne!(base, key(&p), "schema version");
        let mut p = parts("g1", &d);
        p.profile = "c|fp|other-model|12000";
        assert_ne!(base, key(&p), "profile");
        // Boundaries are unambiguous.
        let ab = vec!["ab".to_string()];
        let a_b = vec!["a".to_string(), "b".to_string()];
        assert_ne!(key(&parts("g", &ab)), key(&parts("g", &a_b)));
    }

    #[test]
    fn changed_or_revoked_grants_change_the_stamp() {
        let ws = vec!["/w".to_string()];
        let base = grants_stamp(&[grant("/w", 1, &["structured_state"])], &ws, "c");
        assert_eq!(
            base,
            grants_stamp(&[grant("/w", 1, &["structured_state"])], &ws, "c")
        );
        assert_ne!(
            base,
            grants_stamp(&[grant("/w", 2, &["structured_state"])], &ws, "c"),
            "regranted"
        );
        assert_ne!(
            base,
            grants_stamp(&[grant("/w", 1, &["structured_state", "screen"])], &ws, "c"),
            "widened"
        );
        assert_ne!(base, grants_stamp(&[], &ws, "c"), "revoked");
        let mut changed = grant("/w", 1, &["structured_state"]);
        changed.fingerprint = "fp2".into();
        assert_ne!(base, grants_stamp(&[changed], &ws, "c"), "endpoint changed");
        // Another workspace's grant never matters, and a second workspace does.
        assert_eq!(
            base,
            grants_stamp(
                &[grant("/w", 1, &["structured_state"]), grant("/x", 9, &[])],
                &ws,
                "c"
            )
        );
        let two = vec!["/w".to_string(), "/x".to_string()];
        assert_ne!(
            base,
            grants_stamp(&[grant("/w", 1, &["structured_state"])], &two, "c")
        );
    }

    fn entry(k: &str, ws: &str, exp: i64) -> Entry {
        Entry {
            key: k.into(),
            operation: "briefing".into(),
            workspaces: vec![ws.into()],
            created_ms: 0,
            expires_ms: exp,
            data: json!({"output": {"items": []}}),
        }
    }

    #[test]
    fn hits_expire_and_purge_by_workspace() {
        let mut c = Cache::default();
        c.put(entry("k1", "/a", 100));
        c.put(entry("k2", "/b", 200));
        assert!(c.get("k1", 50).is_some());
        assert!(c.get("k1", 100).is_none(), "expired entries never hit");
        assert!(c.get("nope", 0).is_none());
        assert_eq!(c.sweep(150), 1);
        assert_eq!(c.entries.len(), 1);
        c.put(entry("k3", "/a", 500));
        assert_eq!(c.purge_workspace("/a"), 1);
        assert!(c.get("k3", 0).is_none());
        assert!(c.get("k2", 0).is_some());
        assert_eq!(c.purge_all(), 1);
        let mut early = entry("a", "/a", 10_000);
        early.created_ms = 300;
        c.put(early);
        let mut late = entry("b", "/a", 10_000);
        late.created_ms = 500;
        c.put(late);
        assert_eq!(c.purge_before(100), 0);
        assert_eq!(
            c.purge_before(400),
            1,
            "only entries created before the cutoff"
        );
        assert!(c.get("b", 0).is_some());
    }

    #[test]
    fn put_replaces_and_the_cache_is_bounded() {
        let mut c = Cache::default();
        for i in 0..(MAX_ENTRIES + 10) {
            c.put(entry(&format!("k{i}"), "/a", 1000));
        }
        assert_eq!(c.entries.len(), MAX_ENTRIES);
        assert!(c.get("k0", 0).is_none(), "oldest dropped");
        assert!(c.get(&format!("k{}", MAX_ENTRIES + 9), 0).is_some());
        let n = c.entries.len();
        c.put(entry("k100", "/z", 2000));
        assert_eq!(c.entries.len(), n, "same key replaces");
    }
}
