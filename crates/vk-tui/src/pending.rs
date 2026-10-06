//! Durable pending client operations (15 §10.3): every task mutation is written to a small JSON
//! file under the client's state dir **before** it is dispatched, keyed by its idempotency key.
//! After a crash, restart or link drop the client asks the owner (`task.operation.get`) what
//! happened; a mutation whose outcome is unknown is never resubmitted automatically — the user
//! gets an explicit Retry that warns the earlier request may have taken effect.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    /// Dispatched (or about to be); no response observed yet.
    #[default]
    InFlight,
    /// The owner has no receipt (or it expired): the outcome is unknown.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingOp {
    pub key: String,
    pub method: String,
    pub params: Value,
    /// Machine label (stable across client restarts).
    pub machine: String,
    pub created_at_ms: i64,
    #[serde(default)]
    pub status: OpStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl PendingOp {
    /// Short human description ("Track task “Fix login”").
    pub fn describe(&self) -> String {
        let title = self
            .params
            .get("title")
            .and_then(Value::as_str)
            .map(|t| format!(" “{}”", crate::draw::truncate(t, 40)))
            .unwrap_or_default();
        let what = match self.method.as_str() {
            "task.track" => "Track task",
            "task.intent.update" => "Save task details",
            "task.bind" => "Continue task",
            "task.message.prepare" => "Prepare message",
            "task.message.send" => "Send message",
            "task.review.accept" => "Mark reviewed",
            "task.check.run" => "Run check",
            other => other,
        };
        format!("{what}{title}")
    }
}

/// Default location: `$VIBEKE_STATE_DIR/<session>/client-pending.json` (same root as the server's
/// per-session state: `$XDG_STATE_HOME/vibeke` or `~/.local/state/vibeke`).
pub fn default_path(session: &str) -> PathBuf {
    let root = if let Some(d) = std::env::var_os("VIBEKE_STATE_DIR") {
        PathBuf::from(d)
    } else if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(d).join("vibeke")
    } else {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".local/state/vibeke")
    };
    root.join(session).join("client-pending.json")
}

#[derive(Debug, Default)]
pub struct PendingStore {
    /// `None`: memory only (tests that don't care about persistence).
    pub path: Option<PathBuf>,
    pub ops: Vec<PendingOp>,
    /// Keys dispatched over the current connection (a response may still arrive).
    pub live: HashSet<String>,
    /// Keys with a `task.operation.get` outstanding.
    pub reconciling: HashSet<String>,
    /// Recent reconciliation results shown in the pending-operations popup.
    pub outcomes: Vec<String>,
    pub load_error: Option<String>,
}

impl PendingStore {
    pub fn load(path: PathBuf) -> Self {
        let mut s = PendingStore {
            path: Some(path.clone()),
            ..Default::default()
        };
        match std::fs::read(&path) {
            Ok(b) => match serde_json::from_slice::<Vec<PendingOp>>(&b) {
                Ok(ops) => s.ops = ops,
                Err(e) => s.load_error = Some(format!("{}: {e}", path.display())),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => s.load_error = Some(format!("{}: {e}", path.display())),
        }
        s
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = tmp_path(path);
        let data = serde_json::to_vec_pretty(&self.ops).map_err(std::io::Error::other)?;
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&data)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    /// Persist `op` before dispatch. On error nothing is recorded and the caller must not send.
    pub fn add(&mut self, op: PendingOp) -> std::io::Result<()> {
        let key = op.key.clone();
        self.ops.retain(|o| o.key != key);
        self.ops.push(op);
        if let Err(e) = self.save() {
            self.ops.retain(|o| o.key != key);
            return Err(e);
        }
        self.live.insert(key);
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<&PendingOp> {
        self.ops.iter().find(|o| o.key == key)
    }

    /// The owner answered (success or a definitive error): forget the operation.
    pub fn remove(&mut self, key: &str) {
        self.live.remove(key);
        self.reconciling.remove(key);
        let before = self.ops.len();
        self.ops.retain(|o| o.key != key);
        if self.ops.len() != before {
            let _ = self.save();
        }
    }

    pub fn mark_unknown(&mut self, key: &str, note: impl Into<String>) {
        self.reconciling.remove(key);
        self.live.remove(key);
        if let Some(o) = self.ops.iter_mut().find(|o| o.key == key) {
            o.status = OpStatus::Unknown;
            o.note = Some(note.into());
        }
        let _ = self.save();
    }

    /// The link to `machine` dropped: anything dispatched over it now needs reconciliation.
    pub fn connection_lost(&mut self, machine: &str) {
        let keys: Vec<String> = self
            .ops
            .iter()
            .filter(|o| o.machine == machine)
            .map(|o| o.key.clone())
            .collect();
        for k in keys {
            self.live.remove(&k);
            self.reconciling.remove(&k);
        }
    }

    /// Operations for `machine` whose outcome must be asked for (not live, not already asked,
    /// and not already known to be unknown).
    pub fn needs_reconcile(&self, machine: &str) -> Vec<String> {
        self.ops
            .iter()
            .filter(|o| {
                o.machine == machine
                    && o.status == OpStatus::InFlight
                    && !self.live.contains(&o.key)
                    && !self.reconciling.contains(&o.key)
            })
            .map(|o| o.key.clone())
            .collect()
    }

    pub fn unknown_count(&self) -> usize {
        self.ops
            .iter()
            .filter(|o| o.status == OpStatus::Unknown)
            .count()
    }

    pub fn push_outcome(&mut self, s: String) {
        self.outcomes.push(s);
        if self.outcomes.len() > 10 {
            self.outcomes.remove(0);
        }
    }
}

fn tmp_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn op(key: &str, machine: &str) -> PendingOp {
        PendingOp {
            key: key.into(),
            method: "task.track".into(),
            params: json!({"run": "r1", "title": "Fix login", "idempotency_key": key}),
            machine: machine.into(),
            created_at_ms: 1,
            status: OpStatus::InFlight,
            note: None,
        }
    }

    #[test]
    fn persisted_before_dispatch_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s/client-pending.json");
        let mut s = PendingStore::load(path.clone());
        s.add(op("k1", "devbox")).unwrap();
        assert!(path.exists());
        // Live in this session: not reconciled while a response may still arrive.
        assert!(s.needs_reconcile("devbox").is_empty());
        let s2 = PendingStore::load(path.clone());
        assert_eq!(s2.ops.len(), 1);
        assert_eq!(s2.ops[0].key, "k1");
        assert_eq!(s2.needs_reconcile("devbox"), vec!["k1".to_string()]);
        assert!(s2.needs_reconcile("other").is_empty());
        let mut s2 = s2;
        s2.remove("k1");
        assert!(PendingStore::load(path).ops.is_empty());
    }

    #[test]
    fn unknown_is_not_reconciled_again_and_link_loss_requeues() {
        let mut s = PendingStore::default();
        s.add(op("a", "m")).unwrap();
        s.add(op("b", "m")).unwrap();
        s.connection_lost("m");
        assert_eq!(s.needs_reconcile("m").len(), 2);
        s.mark_unknown("a", "no receipt");
        assert_eq!(s.needs_reconcile("m"), vec!["b".to_string()]);
        assert_eq!(s.unknown_count(), 1);
    }

    #[test]
    fn failed_persist_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the directory should be makes create_dir_all fail.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let mut s = PendingStore::load(blocker.join("client-pending.json"));
        assert!(s.add(op("k", "m")).is_err());
        assert!(s.ops.is_empty());
        assert!(s.live.is_empty());
    }
}
