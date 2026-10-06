//! Deterministic core of spec 15 (task outcomes, review packages and the attention inbox),
//! stages T1–T3.
//!
//! This crate is a pure library: it owns the data shapes and decision rules, and leaves storage,
//! JSON-RPC and rendering to the server. The only I/O lives in [`subject`] (read-only `git`
//! invocations) and [`checks`] (running an explicitly authorized check in a disposable checkout).
//!
//! Modules:
//! - [`intent`]: confirmed `TaskIntent` revisions, stable criterion ids/versions and
//!   communication coverage (§2.3, §4.1).
//! - [`binding`]: task ↔ run bindings and their boundary rules (§4.2).
//! - [`subject`]: content-addressed `ChangeSubject`s, observation baselines and review-base
//!   proposals (§5).
//! - [`checks`]: check definitions with resolved-script digests, per-candidate authorization,
//!   disposable-checkout execution and observed agent commands (§6.2, §6.3).
//! - [`readiness`]: criterion assessment, readiness labels, acceptance and freshness (§6.1, §7).
//! - [`screenshot`]: screenshot code state, running-build identity and their binding (06 B6,
//!   §6.4).
//! - [`attention`]: inbox ranking, five-minute view, snooze wake-ups, stable selection and
//!   batching (§8).
//!
//! T4 additions:
//! - [`snapshot`]: validated, content-addressed dirty-work snapshots stored as immutable Git
//!   commits under `refs/vibeke/snapshots/` (§5).
//! - [`dependency`]: user-confirmed dependency edges with cycle checks (§8.1).
//! - [`effort`]: the deterministic review-effort heuristic (§8.2).
//! - [`reviewer`]: reviewable reviewer-run prompts and reviewer findings as attributed notes
//!   (§6.1, §7).
//! - [`selection`]: selected-patch snapshots (whole files or a patch on top of HEAD) as
//!   immutable, accept-capable review subjects (§5).
//! - [`scratch`]: disposable reviewer checkouts, so a reviewer is never a writer in the task's
//!   checkout (§6.1).
//!
//! Lane 3F additions:
//! - [`pr_evidence`]: pull-request observations, claims and their binding to a committed subject
//!   (§6.4).
//! - [`interval`]: execution-interval binding of observed commands through a write journal and
//!   start/end checkout states (§6.3).
//!
//! Conventions: timestamps are Unix epoch milliseconds (`i64`, fields suffixed `_ms`, as in
//! `vk-proto`); every enum serializes as `snake_case`; ids are opaque strings (ULIDs where this
//! crate mints them).

pub mod attention;
pub mod binding;
pub mod checks;
pub mod dependency;
pub mod effort;
pub mod intent;
pub mod interval;
pub mod pr_evidence;
pub mod readiness;
pub mod reviewer;
pub mod scratch;
pub mod screenshot;
pub mod selection;
pub mod snapshot;
pub mod subject;

mod gitcmd;

use serde::{Deserialize, Serialize};

/// Who performed or confirmed something.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Actor {
    pub kind: ActorKind,
    /// User name, client id, run id or recipe id depending on `kind`.
    pub id: String,
}

impl Actor {
    pub fn user(id: impl Into<String>) -> Self {
        Actor {
            kind: ActorKind::User,
            id: id.into(),
        }
    }
    pub fn agent(id: impl Into<String>) -> Self {
        Actor {
            kind: ActorKind::Agent,
            id: id.into(),
        }
    }
    pub fn system(id: impl Into<String>) -> Self {
        Actor {
            kind: ActorKind::System,
            id: id.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    /// A human acting through an explicit human-client scope.
    User,
    /// An agent/adapter reporting its own observations. Can never confirm or accept (§11).
    Agent,
    /// A trusted project recipe.
    ProjectRecipe,
    /// Optional assistance (spec 14); its outputs are drafts only.
    Assistant,
    /// Vibeke itself (e.g. an automatic continuation edge).
    System,
}

/// Provenance of a requirement or excerpt (§4.1): machine/session/run/turn/item and, where
/// applicable, a source digest.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SourceRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// Native conversation the source belongs to (for communication coverage, §2.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// The source is a user message that was already delivered to `native_conversation_id`.
    #[serde(default)]
    pub delivered_user_message: bool,
    /// Who supplied this source (handwritten requirements have user provenance; generated
    /// drafts carry their assistant result reference in `item`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Actor>,
}

/// Current wall clock as epoch milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A fresh ULID string.
pub(crate) fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

/// Length-prefixed field hasher used for every content-addressed digest in this crate, so that
/// `("ab","c")` and `("a","bc")` never collide.
pub(crate) struct FieldHasher(blake3::Hasher);

impl FieldHasher {
    pub(crate) fn new(domain: &str) -> Self {
        let mut h = FieldHasher(blake3::Hasher::new());
        h.field(domain.as_bytes());
        h
    }
    pub(crate) fn field(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(&(bytes.len() as u64).to_le_bytes());
        self.0.update(bytes);
        self
    }
    pub(crate) fn str(&mut self, s: &str) -> &mut Self {
        self.field(s.as_bytes())
    }
    pub(crate) fn opt(&mut self, s: Option<&str>) -> &mut Self {
        match s {
            None => self.field(&[0]),
            Some(v) => {
                self.field(&[1]);
                self.str(v)
            }
        }
    }
    pub(crate) fn finish(&self) -> String {
        self.0.finalize().to_hex().to_string()
    }
}

/// Truncate to at most `max` bytes on a char boundary.
pub(crate) fn truncate_utf8(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}
