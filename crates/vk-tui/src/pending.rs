//! Durable pending client operations (15 §10.3): every task mutation is written to a small JSON
//! file under the client's state dir **before** it is dispatched, keyed by its idempotency key.
//! Each client instance owns one file (`client-pending-<client_id>.json`) and holds an advisory
//! lock on a sibling `.live` file while it runs; on start (and reconnect) a client adopts the
//! files of clients that no longer hold theirs (crashed), under a directory-wide lock. No client
//! rewrites or deletes another live client's entries.
//! After a crash, restart or link drop the client asks the owner (`task.operation.get`) what
//! happened; a mutation whose outcome is unknown is never resubmitted automatically — the user
//! gets an explicit Retry that warns the earlier request may have taken effect.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

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
            "task.review.snapshot" => "Snapshot uncommitted work",
            "task.review.request_reviewer" => "Prepare reviewer prompt",
            "task.review.start_reviewer" => "Start reviewer",
            "task.review.note.classify" => "Classify review note",
            "task.dependency.add" => "Add dependency link",
            "task.dependency.remove" => "Remove dependency link",
            "task.set" => "Set task effort",
            "draft.send" => "Send draft",
            other => other,
        };
        format!("{what}{title}")
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[path = "pending/native.rs"]
mod storage;
#[cfg(target_arch = "wasm32")]
#[path = "pending/browser.rs"]
mod storage;
#[cfg(not(target_arch = "wasm32"))]
pub use storage::{default_dir, read_ops};

#[derive(Debug, Default)]
pub struct PendingStore {
    storage: storage::Storage,
    pub ops: Vec<PendingOp>,
    /// Keys dispatched over the current connection (a response may still arrive).
    pub live: HashSet<String>,
    /// Keys with a `task.operation.get` outstanding.
    pub reconciling: HashSet<String>,
    /// Recent reconciliation results shown in the pending-operations popup.
    pub outcomes: Vec<String>,
    pub load_error: Option<String>,
    /// Number of operations adopted from crashed clients' files.
    pub adopted: usize,
}

impl PendingStore {
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
