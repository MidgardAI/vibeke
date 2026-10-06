//! Spec 15 T2 (evidence-backed review) and T3 (attention ranking), server side.
//!
//! The decision rules live in `vk-review`; this module gathers real data for them, persists
//! the resulting objects and serves the API:
//!
//! - **Change subjects** (§5): committed candidates (`review base..HEAD` of the task's checkout
//!   while a binding is live; pinned end candidates of closed bindings), dirty work as an
//!   inspect-only live subject.
//! - **Observed command table** (§6.2) from the task's bound turns only, plus the agent's last
//!   message shown as a claim.
//! - **Checks** (§6.3) from `[[task.checks]]` in the candidate's `.vibeke/config.toml`, resolved
//!   at the candidate commit, run only after a per-candidate user authorization, in a disposable
//!   checkout on a background thread.
//! - **Review package, readiness and acceptance** (§6–§7) with live state from the model.
//! - **Attention** (§8): ranked items across interactions, tasks, checks, sends and finished
//!   turns, with per-user seen/snooze/pin preferences.
//!
//! Git work and check execution never run under the core lock or on the render path: API
//! handlers use `spawn_blocking`; background refreshes go through one bounded, coalescing
//! queue (at most [`MAX_REFRESH_WORKERS`] threads, one refresh per task at a time).
//!
//! Reads are authorized before retrieval (15 §11): pane-token callers only see tasks, checks
//! and attention items of their pane's workspace.

pub mod attention_ext;
pub mod ext;
pub mod human;
pub mod link;
pub mod patch;
pub mod purge;
pub mod receipts;
pub mod scratch;
pub mod t4;

use crate::Server;
use crate::api::{Ctx, R, err, internal, invalid, not_found, req, s, u};
use crate::core::{Core, Tx};
use crate::tracking::{self, MessageState};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use vk_proto::model::*;
use vk_proto::rpc::{ErrorKind, RpcError};
use vk_review::attention::{self as att, AttentionItem, AttentionKind, Effort, ItemKey};
use vk_review::binding::{BindingRole, BindingState, TaskRunBinding};
use vk_review::checks::{
    self, AuthRequirement, CheckCommand, CheckDefinition, CheckGrant, CheckRun, CheckSpec,
    CheckState, CheckTrust, ObservedCommand, ToolRecord, ToolRecordSource,
};
use vk_review::intent::{Evaluation, TaskIntent};
use vk_review::readiness::{
    self, AcceptRequest, BoundRun, CriterionException, Evidence, FreshnessState, LiveState,
    PackageSnapshot, ReadinessLabel, ReviewAcceptance, ReviewAssessment, RunActivity,
};
use vk_review::subject::{self, ChangeSubject, DirtyState, SubjectKind};

const K_SUBJECT: &str = "review_subject";
const K_CAND: &str = "review_candidate";
const K_END: &str = "binding_end";
const K_PROJ: &str = "review_projection";
const K_ACCEPT: &str = "review_acceptance";
const K_GRANT: &str = "check_grant";
const K_CHECK: &str = "check_run";
const K_PREF: &str = "attention_pref";

/// Default check timeout when a recipe does not set one.
const DEFAULT_CHECK_TIMEOUT_MS: u64 = 10 * 60 * 1000;
/// Background review refreshes run on at most this many threads (15 §11 bounded queues).
pub const MAX_REFRESH_WORKERS: usize = 2;
/// `task.review.diff` default and maximum response sizes.
const DIFF_DEFAULT_BYTES: usize = 256 * 1024;
const DIFF_MAX_BYTES: usize = 4 * 1024 * 1024;

// ---- records ------------------------------------------------------------------------------------

/// A subject that was a review candidate of a task (keyed `task:subject`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateRec {
    pub task: String,
    pub subject_id: String,
    pub head_sha: String,
    pub base_sha: String,
    /// `current_head` (live checkout HEAD) or `binding_end` (pinned when a binding closed).
    pub source: String,
    pub binding: Option<String>,
    pub created_at_ms: i64,
}

/// What a binding pinned when it closed (15 §4.2): a committed candidate or none.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndRec {
    pub binding: String,
    pub task: String,
    pub run: String,
    pub subject_id: Option<String>,
    pub head_sha: Option<String>,
    pub note: Option<String>,
    pub at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FailedCheck {
    pub run: String,
    pub check: String,
    pub state: CheckState,
    pub ended_at_ms: i64,
}

/// Cached per-task review projection used by `attention.list` (no git on that path).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Projection {
    pub task: String,
    pub label: String,
    pub label_text: String,
    pub subject_id: Option<String>,
    pub head_sha: Option<String>,
    pub candidate_at_ms: Option<i64>,
    pub intent_revision: Option<u32>,
    pub package_revision: u64,
    /// Monotonic material revision (bumped when the package or label changes).
    pub revision: u64,
    pub failed_checks: Vec<FailedCheck>,
    pub explanation: Option<String>,
    pub updated_at_ms: i64,
    /// Token of the live state (runs, writers, interactions, sends, switches) the label was
    /// computed under. A reader whose current token differs must not show a cached Ready.
    #[serde(default)]
    pub live_token: Option<u64>,
    /// Deterministic effort heuristic of the current candidate (T4, §8.2), shown as an
    /// estimate labelled `heuristic`; never used as the user's effort.
    #[serde(default)]
    pub effort_heuristic: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptanceRec {
    pub acceptance: ReviewAcceptance,
    pub outdated_at_ms: Option<i64>,
    pub outdated_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrantRec {
    pub task: String,
    pub grant: CheckGrant,
    pub definition: CheckDefinition,
    pub head_sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckRunRec {
    pub task: String,
    pub run: CheckRun,
    pub definition: CheckDefinition,
    pub subject: ChangeSubject,
}

/// Per-user inbox preference for one item (15 §8.3), keyed `kind:id`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Pref {
    pub key: String,
    pub seen_rev: Option<u64>,
    pub snoozed_until_ms: Option<i64>,
    /// The item as it was when snoozed (for material wake-ups).
    pub snapshot: Option<AttentionItem>,
    pub pinned: bool,
    pub updated_at_ms: i64,
}

// ---- small helpers ------------------------------------------------------------------------------

fn now() -> i64 {
    vk_store::now_ms()
}

fn cancels() -> &'static Mutex<HashMap<String, checks::CancelToken>> {
    static M: OnceLock<Mutex<HashMap<String, checks::CancelToken>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// Every record of a task-scoped kind (`task` field) for one task: open and closed, no global
/// history limit (15 §11; indexed lookup).
fn by_task<T: DeserializeOwned>(c: &Core, kind: &str, task: &str) -> Vec<T> {
    c.store.load_by_task(kind, task).unwrap_or_default()
}

fn acceptances_of(c: &Core, task: &str) -> Vec<AcceptanceRec> {
    c.store
        .load_by_field(K_ACCEPT, "$.acceptance.task_id", task)
        .unwrap_or_default()
}

/// Every recorded acceptance of a task (current and outdated), for screenshot retention
/// (06 B6: evidence referenced by an acceptance is retained, 15 §11).
pub fn acceptances_for(server: &Server, task: &str) -> Vec<ReviewAcceptance> {
    server.with_core(|c| {
        acceptances_of(c, task)
            .into_iter()
            .map(|a| a.acceptance)
            .collect()
    })
}

fn bindings_of(c: &Core, task: &str) -> Vec<TaskRunBinding> {
    let mut v: Vec<TaskRunBinding> = c
        .store
        .load_by_field(tracking::K_BINDING, "$.task_id", task)
        .unwrap_or_default();
    v.sort_by_key(|b| b.created_at_ms);
    v
}

// ---- bounded, coalescing refresh queue (15 §11) ---------------------------------------------

struct RefreshJob {
    server: Weak<Server>,
    task: String,
    not_before: Instant,
}

#[derive(Default)]
struct RefreshQueue {
    jobs: VecDeque<RefreshJob>,
    queued: HashSet<String>,
    running: HashSet<String>,
    /// Tasks asked to refresh again while their refresh was running.
    again: HashMap<String, Weak<Server>>,
    workers: usize,
}

fn refresh_queue() -> &'static (Mutex<RefreshQueue>, Condvar) {
    static Q: OnceLock<(Mutex<RefreshQueue>, Condvar)> = OnceLock::new();
    Q.get_or_init(Default::default)
}

/// Queue a refresh of `task` after `delay`. At most one refresh per task runs at a time; a
/// request while it runs coalesces into one follow-up; requests for an already queued task
/// are dropped. Never spawns more than [`MAX_REFRESH_WORKERS`] threads.
fn enqueue_refresh(server: &Arc<Server>, task: &str, delay: Duration) {
    let (m, cv) = refresh_queue();
    let mut q = m.lock().unwrap();
    if q.running.contains(task) {
        q.again.insert(task.to_string(), Arc::downgrade(server));
        return;
    }
    if !q.queued.insert(task.to_string()) {
        return;
    }
    q.jobs.push_back(RefreshJob {
        server: Arc::downgrade(server),
        task: task.to_string(),
        not_before: Instant::now() + delay,
    });
    if q.workers < MAX_REFRESH_WORKERS {
        q.workers += 1;
        let spawned = std::thread::Builder::new()
            .name("vk-review-refresh".into())
            .spawn(refresh_worker);
        if spawned.is_err() {
            q.workers -= 1;
        }
    }
    cv.notify_one();
}

fn refresh_worker() {
    let (m, cv) = refresh_queue();
    loop {
        let job = {
            let mut q = m.lock().unwrap();
            loop {
                let now = Instant::now();
                if let Some(i) = q.jobs.iter().position(|j| j.not_before <= now) {
                    let job = q.jobs.remove(i).expect("position is valid");
                    q.queued.remove(&job.task);
                    q.running.insert(job.task.clone());
                    break job;
                }
                let wait = q
                    .jobs
                    .iter()
                    .map(|j| j.not_before.saturating_duration_since(now))
                    .min();
                match wait {
                    Some(w) => q = cv.wait_timeout(q, w).unwrap().0,
                    None => {
                        let (g, to) = cv.wait_timeout(q, Duration::from_secs(10)).unwrap();
                        q = g;
                        if to.timed_out() && q.jobs.is_empty() {
                            q.workers -= 1;
                            return;
                        }
                    }
                }
            }
        };
        if let Some(srv) = job.server.upgrade() {
            let task = job.task.clone();
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Ok(pkg) = build_package(&srv, &task, None) {
                    persist(&srv, &pkg);
                    ensure_watcher(&srv);
                }
            }));
        }
        let mut q = m.lock().unwrap();
        q.running.remove(&job.task);
        if let Some(w) = q.again.remove(&job.task)
            && q.queued.insert(job.task.clone())
        {
            q.jobs.push_back(RefreshJob {
                server: w,
                task: job.task,
                not_before: Instant::now(),
            });
            cv.notify_one();
        }
    }
}

/// Test/diagnostic: whether any refresh is queued or running for `task`.
pub fn refresh_pending(task: &str) -> bool {
    let q = refresh_queue().0.lock().unwrap();
    q.queued.contains(task) || q.running.contains(task) || q.again.contains_key(task)
}

/// Test/diagnostic: how many refresh worker threads exist right now.
pub fn refresh_workers() -> usize {
    refresh_queue().0.lock().unwrap().workers
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(f).await.map_err(internal)
}

fn conflict(reason: &str, msg: impl Into<String>) -> RpcError {
    tracking::conflict(reason, msg)
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn command_text(c: &CheckCommand) -> String {
    match c {
        CheckCommand::Argv(v) => v.join(" "),
        CheckCommand::Shell(s) => s.clone(),
    }
}

/// Read-only blob lookup at a commit (`git cat-file blob <sha>:<path>`).
fn git_show(root: &Path, sha: &str, rel: &str) -> Option<Vec<u8>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "blob", &format!("{sha}:{rel}")])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

fn label_key(l: ReadinessLabel, acc: Option<&ReviewAcceptance>) -> &'static str {
    match l {
        ReadinessLabel::TurnFinished => "turn_finished",
        ReadinessLabel::NeedsTaskDetails => "needs_task_details",
        ReadinessLabel::ChangesToInspect => "changes_to_inspect",
        ReadinessLabel::ReviewAvailable => "review_available",
        ReadinessLabel::ReadyForReview => "ready_for_review",
        ReadinessLabel::Reviewed if acc.is_some_and(|a| a.with_exceptions()) => {
            "reviewed_with_exceptions"
        }
        ReadinessLabel::Reviewed => "reviewed",
        ReadinessLabel::ReviewOutdated => "review_outdated",
    }
}

fn label_text(key: &str) -> &'static str {
    match key {
        "turn_finished" => "Turn finished",
        "needs_task_details" => "Needs task details",
        "changes_to_inspect" => "Changes to inspect",
        "review_available" => "Review available",
        "ready_for_review" => "Ready for your review",
        "reviewed_with_exceptions" => "Reviewed with exceptions",
        "reviewed" => "Reviewed",
        "review_outdated" => "Review outdated",
        _ => "",
    }
}

// ---- check recipes ------------------------------------------------------------------------------

/// `[[task.checks]]` (or top-level `[[checks]]`) entries of a `.vibeke/config.toml`:
/// `name`, `command` (string → `/bin/sh -c`, array → argv), optional `id`, `cwd`, `timeout_s`,
/// `env`.
pub fn parse_checks(text: &str) -> Vec<CheckSpec> {
    let Ok(v) = text.parse::<toml::Table>() else {
        return vec![];
    };
    let arr = v
        .get("task")
        .and_then(|t| t.get("checks"))
        .and_then(|c| c.as_array())
        .or_else(|| v.get("checks").and_then(|c| c.as_array()));
    let mut out = Vec::new();
    for e in arr.into_iter().flatten() {
        let Some(name) = e.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let command = match e.get("command") {
            Some(toml::Value::String(s)) => CheckCommand::Shell(s.clone()),
            Some(toml::Value::Array(a)) => CheckCommand::Argv(
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect(),
            ),
            _ => continue,
        };
        let timeout_ms = e
            .get("timeout_ms")
            .and_then(|t| t.as_integer())
            .map(|t| t.max(1) as u64)
            .or_else(|| {
                e.get("timeout_s")
                    .and_then(|t| t.as_integer())
                    .map(|t| t.max(1) as u64 * 1000)
            })
            .unwrap_or(DEFAULT_CHECK_TIMEOUT_MS);
        let env = e
            .get("env")
            .and_then(|t| t.as_table())
            .map(|t| {
                t.iter()
                    .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        out.push(CheckSpec {
            id: e
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or(name)
                .to_string(),
            name: name.to_string(),
            command,
            cwd: e
                .get("cwd")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            env,
            timeout_ms,
            trust: CheckTrust::ProjectRecipe,
        });
    }
    out
}

fn check_specs_at(root: &Path, sha: &str) -> Vec<CheckSpec> {
    git_show(root, sha, ".vibeke/config.toml")
        .map(|b| parse_checks(&String::from_utf8_lossy(&b)))
        .unwrap_or_default()
}

/// A check definition resolved at a candidate, with its provenance against the review base.
#[derive(Debug, Clone)]
pub struct CheckEntry {
    pub def: CheckDefinition,
    pub baseline_digest: Option<String>,
    pub provenance: Value,
}

/// Resolve every recipe check at `subject.head_sha` and classify it against the review base
/// (`subject.base_sha`): a definition that is new or differs there is `task_modified` (15 §6.3).
pub fn resolve_checks(subject: &ChangeSubject) -> Vec<CheckEntry> {
    let root = PathBuf::from(&subject.repo.root);
    let specs = check_specs_at(&root, subject.content_sha());
    if specs.is_empty() {
        return vec![];
    }
    let base_specs = check_specs_at(&root, &subject.base_sha);
    let base_subject = ChangeSubject::new(
        subject.repo.clone(),
        subject.base_sha.clone(),
        subject.base_sha.clone(),
        None,
        DirtyState::Clean,
        SubjectKind::Committed,
        now(),
    );
    let mut out = Vec::new();
    for spec in specs {
        let Ok(mut def) = checks::resolve_definition_at(subject, &spec) else {
            continue;
        };
        let base_spec = base_specs.iter().find(|b| b.id == spec.id);
        let base_def = base_spec.and_then(|b| checks::resolve_definition_at(&base_subject, b).ok());
        let base_digest = base_def.as_ref().map(|d| d.definition_digest.clone());
        def.trust = checks::classify_trust(
            CheckTrust::ProjectRecipe,
            base_digest.as_deref(),
            &def.definition_digest,
        );
        let changed: Vec<Value> = def
            .resolved
            .iter()
            .filter(|r| {
                base_def
                    .as_ref()
                    .and_then(|b| {
                        b.resolved
                            .iter()
                            .find(|x| x.kind == r.kind && x.file == r.file && x.name == r.name)
                    })
                    .map(|x| &x.body)
                    != Some(&r.body)
            })
            .map(|r| json!({"kind": r.kind, "file": r.file, "name": r.name}))
            .collect();
        let command_changed = base_spec.is_none_or(|b| b.command != spec.command);
        let note = match (base_spec.is_some(), def.trust) {
            (false, _) => "Check defined by this task's changes; it cannot authorize itself",
            (true, CheckTrust::TaskModified) => {
                "Check definition changed by this task's changes; needs fresh authorization"
            }
            _ => "Unchanged from the review base",
        };
        let provenance = json!({
            "trust": def.trust,
            "definition_digest": def.definition_digest,
            "baseline_digest": base_digest,
            "defined_at": subject.content_sha(),
            "baseline_at": subject.base_sha,
            "defined_at_baseline": base_spec.is_some(),
            "command_changed": command_changed,
            "changed": changed,
            "resolution_complete": def.resolution_complete,
            "note": note,
        });
        out.push(CheckEntry {
            def,
            baseline_digest: base_digest,
            provenance,
        });
    }
    out
}

// ---- candidates (§5) ----------------------------------------------------------------------------

struct Candidates {
    checkout: Option<PathBuf>,
    live: bool,
    base_sha: Option<String>,
    review_base: Value,
    current: Option<ChangeSubject>,
    ends: Vec<(EndRec, Option<ChangeSubject>)>,
    live_subject: Option<ChangeSubject>,
    dirty_state: Option<DirtyState>,
    warnings: Vec<String>,
    /// Further known candidates (T4): the task's dirty snapshots, and the committed candidate
    /// when a matching snapshot supersedes it as current.
    extra: Vec<(ChangeSubject, &'static str)>,
}

fn checkout_of(task: &Task) -> Option<PathBuf> {
    let p = PathBuf::from(task.worktree_path.as_deref().unwrap_or(&task.repo_root));
    p.is_dir().then_some(p)
}

/// The workspace's default branch for review-base proposals (15 §5): `origin/HEAD`'s branch,
/// else a local `main`/`master`. Locally resolvable only; never fetches.
pub fn default_branch(path: &Path) -> Option<String> {
    subject::default_branch(path)
}

/// The proposed review base (15 §5): the base recorded at tracking time, the owned task's
/// resolved base, the merge-base with the workspace default branch, or the HEAD fallback.
/// A recorded HEAD fallback is replaced by the default-branch merge-base when the checkout is
/// on another branch (otherwise tracking already-committed work yields no candidate).
fn base_for(server: &Server, task: &Task, path: &Path) -> (Option<String>, Value) {
    let default = default_branch(path);
    let off_default = || {
        default
            .as_deref()
            .is_some_and(|d| subject::current_branch(path).as_deref() != Some(d))
    };
    let stored = tracking::baseline_of(server, &task.id);
    if let Some(b) = stored
        .as_ref()
        .and_then(|v| v.get("review_base"))
        .filter(|b| !b.is_null())
        && let Some(sha) = b.get("base_sha").and_then(Value::as_str)
        && subject::rev_parse(path, sha).is_ok()
    {
        let head_fallback = b["reason"]["kind"] == "head_fallback";
        if !(head_fallback && off_default()) {
            return (Some(sha.to_string()), b.clone());
        }
        if let Ok(p) = subject::propose_review_base(path, None, None, default.as_deref())
            && matches!(p.reason, subject::BaseReason::MergeBaseWithDefault { .. })
        {
            return (Some(p.base_sha.clone()), json!(p));
        }
        return (Some(sha.to_string()), b.clone());
    }
    let owned = (task.ownership == TaskOwnership::Owned)
        .then_some(task.base_ref.as_deref())
        .flatten();
    match subject::propose_review_base(path, owned, None, default.as_deref()) {
        Ok(p) => (Some(p.base_sha.clone()), json!(p)),
        Err(e) => (None, json!({"error": e.to_string()})),
    }
}

/// Only an **active** binding follows the checkout: a suspended binding (`/clear`, `/new`, a
/// fork …) is not live for candidate purposes (15 §4.2).
fn is_live(task: &Task, bs: &[TaskRunBinding]) -> bool {
    bs.iter().any(|b| b.state == BindingState::Active)
        || (bs.is_empty() && task.ownership == TaskOwnership::Owned)
}

fn candidates_blocking(server: &Server, task: &Task, bs: &[TaskRunBinding]) -> Candidates {
    let checkout = checkout_of(task);
    let live = is_live(task, bs);
    let mut ends: Vec<(EndRec, Option<ChangeSubject>)> = server.with_core(|c| {
        let mut v: Vec<EndRec> = by_task(c, K_END, &task.id);
        v.sort_by_key(|e| e.at_ms);
        v.into_iter()
            .map(|e| {
                let s = e
                    .subject_id
                    .as_ref()
                    .and_then(|id| c.store.get::<ChangeSubject>(K_SUBJECT, id).ok().flatten());
                (e, s)
            })
            .collect()
    });
    ends.dedup_by(|a, b| a.0.binding == b.0.binding);
    let mut out = Candidates {
        checkout: checkout.clone(),
        live,
        base_sha: None,
        review_base: Value::Null,
        current: None,
        ends,
        live_subject: None,
        dirty_state: None,
        warnings: vec![],
        extra: vec![],
    };
    let Some(path) = checkout else {
        // Historically inspectable, not currently verifiable (15 §4.3/§7): the retained end
        // candidate is shown, but sources can't be revalidated, so acceptance is unavailable.
        out.warnings.push(
            "Checkout unavailable; retained candidates are inspect only (sources cannot be verified)"
                .into(),
        );
        out.current = out.ends.iter().rev().find_map(|(_, s)| s.clone());
        return out;
    };
    if !live
        && let Some(b) = bs.iter().find(|b| {
            b.state == BindingState::Suspended && !out.ends.iter().any(|(e, _)| e.binding == b.id)
        })
    {
        out.warnings.push(format!(
            "Conversation changed for binding {}: its end candidate was not pinned; choose a committed range explicitly",
            b.id
        ));
    }
    let (base, review_base) = base_for(server, task, &path);
    out.base_sha = base.clone();
    out.review_base = review_base;
    if live {
        let head = subject::rev_parse(&path, "HEAD").ok();
        if let (Some(base), Some(head)) = (&base, &head)
            && base != head
        {
            match subject::capture_committed(&path, base, head) {
                Ok(s) => out.current = Some(s),
                Err(e) => out.warnings.push(format!("candidate capture failed: {e}")),
            }
        }
        match subject::observation_baseline(&path) {
            Ok(b) => {
                out.dirty_state = Some(b.dirty_state);
                match b.dirty_state {
                    DirtyState::Dirty => {
                        out.live_subject = Some(ChangeSubject::new(
                            b.repo.clone(),
                            base.clone().unwrap_or_default(),
                            head.clone().unwrap_or_default(),
                            b.change_digest.clone(),
                            DirtyState::Dirty,
                            SubjectKind::CheckoutLive,
                            now(),
                        ));
                        // T4: a validated snapshot of exactly this content is the current,
                        // accept-capable candidate (it includes any commits since the base).
                        if let Some(sn) = t4::current_snapshot(
                            server,
                            &task.id,
                            head.as_deref(),
                            b.change_digest.as_deref(),
                        ) {
                            if let Some(committed) = out.current.take() {
                                out.extra.push((committed, "current_head"));
                            }
                            out.current = Some(sn);
                        }
                        // Lane 2C: a newer selected patch that still matches is current.
                        patch::apply_current(server, &task.id, &path, &mut out);
                    }
                    DirtyState::Unknown => out
                        .warnings
                        .push("Workspace changing — verification subject unavailable".into()),
                    DirtyState::Clean => {}
                }
            }
            Err(e) => out.warnings.push(format!("checkout capture failed: {e}")),
        }
    } else {
        out.current = out.ends.iter().rev().find_map(|(_, s)| s.clone());
    }
    for sn in t4::snapshot_subjects(server, &task.id) {
        let src = patch::source_of(&sn);
        out.extra.push((sn, src));
    }
    out
}

/// Pin the end-boundary candidate of a binding that just closed or was suspended (15 §4.2), or
/// record that there is none — **synchronously**: the checkout's HEAD is read before this
/// returns, so a commit made by the next task after the boundary cannot become this task's
/// end candidate. Only cheap Git plumbing runs here (`rev-parse`, `merge-base`); the review
/// package is rebuilt later by the refresh queue from the pinned SHA.
///
/// Idempotent per binding: an existing pin is returned unchanged. Call it right after the
/// transaction that closed/suspended the binding has committed, without the core lock held.
pub fn pin_end_candidate_sync(server: &Server, b: &TaskRunBinding) -> Option<EndRec> {
    if let Some(e) = server.with_core(|c| c.store.get::<EndRec>(K_END, &b.id).ok().flatten()) {
        return Some(e);
    }
    let task = tracking::find_task(server, &b.task_id).ok()?;
    let mut rec = EndRec {
        binding: b.id.clone(),
        task: task.id.clone(),
        run: b.run_id.clone(),
        subject_id: None,
        head_sha: None,
        note: None,
        at_ms: now(),
    };
    let mut subj = None;
    let mut end_snap: Option<t4::SnapRec> = None;
    match checkout_of(&task) {
        None => rec.note = Some("No bound end candidate: checkout unavailable".into()),
        Some(path) => {
            // HEAD first: this is the boundary.
            let head = subject::rev_parse(&path, "HEAD").ok();
            let (base, _) = base_for(server, &task, &path);
            match (base, head) {
                (Some(base), Some(head)) if base != head => {
                    match subject::capture_committed(&path, &base, &head) {
                        Ok(s) => {
                            rec.subject_id = Some(s.id.clone());
                            rec.head_sha = Some(s.head_sha.clone());
                            subj = Some(s);
                        }
                        Err(e) => rec.note = Some(format!("No bound end candidate: {e}")),
                    }
                }
                (_, None) => {
                    rec.note = Some("No bound end candidate: HEAD unavailable".into());
                }
                _ => {
                    rec.note = Some(
                        "No bound end candidate: no commits since the review base; uncommitted work can't be a candidate — choose a committed range later"
                            .into(),
                    )
                }
            }
            // T4 (lane 2C): uncommitted work at the boundary becomes a dirty-snapshot end
            // candidate (it includes the commits since the base).
            match patch::dirty_end(server, &task, &path, b) {
                Ok(Some((s, sr))) => {
                    rec.subject_id = Some(s.id.clone());
                    rec.head_sha = Some(s.head_sha.clone());
                    rec.note = Some(
                        "Pinned a snapshot of the uncommitted work at the boundary (taken as the binding closed)"
                            .into(),
                    );
                    subj = Some(s);
                    end_snap = Some(sr);
                }
                Ok(None) => {}
                Err(why) => {
                    let base_note = if subj.is_some() {
                        "Pinned the committed candidate"
                    } else {
                        "No bound end candidate"
                    };
                    rec.note = Some(format!(
                        "{base_note}: the uncommitted work at the boundary could not be captured ({why})"
                    ));
                }
            }
        }
    }
    let mut c = server.core.lock().unwrap();
    if let Some(e) = c.store.get::<EndRec>(K_END, &b.id).ok().flatten() {
        return Some(e);
    }
    let mut tx = Tx::new();
    if let Some(s) = &subj {
        if c.store
            .get::<Value>(K_SUBJECT, &s.id)
            .ok()
            .flatten()
            .is_none()
        {
            tx.m.close(K_SUBJECT, &s.id, None, s);
        }
        let key = format!("{}:{}", task.id, s.id);
        if c.store.get::<Value>(K_CAND, &key).ok().flatten().is_none() {
            let cr = CandidateRec {
                task: task.id.clone(),
                subject_id: s.id.clone(),
                head_sha: s.head_sha.clone(),
                base_sha: s.base_sha.clone(),
                source: "binding_end".into(),
                binding: Some(b.id.clone()),
                created_at_ms: now(),
            };
            tx.m.close(K_CAND, &key, None, &cr);
            tx.event(
                "review.candidate_created",
                json!({"task": task.id}),
                json!({"subject": s.id, "head": s.head_sha, "source": "binding_end"}),
            );
        }
    }
    if let Some(sr) = &end_snap {
        tx.m.put(
            t4::K_SNAP,
            &format!("{}:{}", task.id, sr.subject_id),
            None,
            sr,
        );
    }
    tx.m.close(K_END, &b.id, None, &rec);
    tx.event(
        "review.end_candidate_pinned",
        json!({"task": task.id, "binding": b.id, "run": b.run_id}),
        json!({"subject": rec.subject_id, "head": rec.head_sha, "note": rec.note}),
    );
    let _ = server.commit(&mut c, tx);
    Some(rec)
}

/// Hook: bindings closed (unbind / switch) or suspended (conversation boundary). Pins each end
/// candidate synchronously (HEAD is read before this returns), then queues the expensive
/// package refresh.
pub fn on_bindings_closed(server: &Arc<Server>, closed: Vec<TaskRunBinding>) {
    let mut tasks = BTreeSet::new();
    for b in &closed {
        pin_end_candidate_sync(server, b);
        tasks.insert(b.task_id.clone());
    }
    for t in tasks {
        spawn_refresh(server, &t);
    }
}

/// Hook: a bound run's turn settled — a checkpoint for a changed candidate (15 §8.1).
pub fn on_turn_settled(server: &Arc<Server>, run: &str) {
    let tasks: BTreeSet<String> = server.with_core(|c| {
        c.store
            .load::<TaskRunBinding>(tracking::K_BINDING)
            .unwrap_or_default()
            .into_iter()
            .filter(|b| b.run_id == run && b.state == BindingState::Active)
            .map(|b| b.task_id)
            .collect()
    });
    // The adapter applies the idle state right after reporting Stop; let it settle first.
    for t in tasks {
        enqueue_refresh(server, &t, Duration::from_millis(300));
    }
    // A reviewer run's settled turn becomes attributed review notes (T4).
    t4::on_reviewer_turn(server, run);
}

/// Recompute a task's package off the state path and persist label/projection/invalidation,
/// through the bounded refresh queue (coalesced per task).
pub fn spawn_refresh(server: &Arc<Server>, task: &str) {
    enqueue_refresh(server, task, Duration::ZERO);
}

/// Recompute and persist a task's package synchronously (blocking; tests and tools).
pub fn refresh_now(server: &Server, task: &str) -> Result<Value, RpcError> {
    let pkg = build_package(server, task, None)?;
    persist(server, &pkg);
    Ok(pkg.json)
}

// ---- observed commands (§6.2) -------------------------------------------------------------------

/// Commands from the task's bound turns only (binding turn ranges respected), and the agent's
/// last message of the latest bound turn as a claim.
fn observed_table(
    server: &Server,
    bs: &[TaskRunBinding],
) -> (Vec<ObservedCommand>, Vec<ObservedCommand>) {
    let runs: BTreeSet<&str> = bs.iter().map(|b| b.run_id.as_str()).collect();
    let covered = |run: &str, turn: u32| bs.iter().any(|b| b.run_id == run && b.covers(turn));
    let mut observed = Vec::new();
    let mut latest: Option<(i64, ToolRecord)> = None;
    for run in runs {
        for it in tracking::items_of(server, run) {
            let Some(t) = it.turn else { continue };
            if covered(run, t)
                && let Some(oc) = ObservedCommand::from_tool_record(&it)
            {
                observed.push(oc);
            }
        }
        for t in tracking::turns_of(server, run, 500) {
            let Some(msg) = t.last_message.clone().filter(|m| !m.trim().is_empty()) else {
                continue;
            };
            if !covered(run, t.n) {
                continue;
            }
            let at = t.ended_at_ms.unwrap_or(t.started_at_ms);
            if latest.as_ref().is_none_or(|(a, _)| at > *a) {
                latest = Some((
                    at,
                    ToolRecord {
                        run_id: run.to_string(),
                        turn: Some(t.n),
                        item_id: format!("{run}:turn{}:claim", t.n),
                        tool: "assistant_message".into(),
                        command: None,
                        cwd: None,
                        exit_code: None,
                        started_at_ms: Some(t.started_at_ms),
                        ended_at_ms: t.ended_at_ms,
                        source: ToolRecordSource::Prose,
                        text: Some(msg),
                        established_subject: None,
                    },
                ));
            }
        }
    }
    observed.sort_by_key(|o| (o.started_at_ms, o.id.clone()));
    let claims = latest
        .and_then(|(_, r)| ObservedCommand::from_tool_record(&r))
        .into_iter()
        .collect();
    (observed, claims)
}

fn command_json(c: &ObservedCommand) -> Value {
    let mut v = serde_json::to_value(c).unwrap_or(Value::Null);
    v["label"] = json!(c.label());
    v["duration_ms"] = json!(match (c.started_at_ms, c.ended_at_ms) {
        (Some(a), Some(b)) => Some(b - a),
        _ => None,
    });
    v["subject"] = json!(c.subject_id.as_deref().unwrap_or("unbound"));
    v
}

// ---- live state ---------------------------------------------------------------------------------

fn canon(p: &str) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p))
}

fn activity(e: &Execution) -> RunActivity {
    match e {
        Execution::Working | Execution::Starting | Execution::RateLimited => RunActivity::Working,
        Execution::Idle => RunActivity::Idle,
        Execution::Exited => RunActivity::Exited,
        Execution::Error | Execution::Unknown => RunActivity::Unknown,
    }
}

/// The model/store facts live readiness depends on, snapshotted under one lock (no Git).
struct LiveSnap {
    runs: Vec<AgentRun>,
    interactions: Vec<Interaction>,
    panes: HashMap<String, Option<String>>,
    /// Open task messages (any state; delivered/cancelled ones are closed rows).
    messages: Vec<tracking::TaskMessage>,
    /// Runs with a queued binding switch.
    pending_runs: BTreeSet<String>,
}

fn live_snap(c: &Core) -> LiveSnap {
    let runs = c.model.runs.clone();
    let pending_runs = runs
        .iter()
        .filter(|r| {
            c.store
                .kv_get("tracking", &tracking::pending_key(&r.id))
                .ok()
                .flatten()
                .is_some()
        })
        .map(|r| r.id.clone())
        .collect();
    LiveSnap {
        interactions: c.model.interactions.clone(),
        panes: c
            .model
            .panes
            .iter()
            .map(|p| (p.id.clone(), p.cwd.clone()))
            .collect(),
        messages: c
            .store
            .load::<tracking::TaskMessage>(tracking::K_MESSAGE)
            .unwrap_or_default(),
        pending_runs,
        runs,
    }
}

/// Live facts for readiness (15 §7): bound runs, other active writers in the same checkout,
/// open interactions on bound runs, pending switches and unresolved deliveries — plus the
/// token identifying exactly these facts (and the bound runs' turn counts). A cached label is
/// only valid while the token is unchanged.
fn live_from(
    snap: &LiveSnap,
    task_id: &str,
    bs: &[TaskRunBinding],
    checkout: Option<&Path>,
) -> (LiveState, u64) {
    let active: Vec<&TaskRunBinding> = bs
        .iter()
        .filter(|b| b.state == BindingState::Active && b.role == BindingRole::Implementation)
        .collect();
    let bound: BTreeSet<&str> = active.iter().map(|b| b.run_id.as_str()).collect();
    let mut h = blake3::Hasher::new();
    let bound_runs: Vec<BoundRun> = bound
        .iter()
        .map(|id| {
            let r = snap.runs.iter().find(|r| r.id == *id);
            let act = r
                .map(|r| activity(&r.execution.value))
                .unwrap_or(RunActivity::Exited);
            h.update(
                format!("run {id} {act:?} {}\n", r.map_or(0, |r| r.turns_completed)).as_bytes(),
            );
            BoundRun {
                run_id: id.to_string(),
                activity: act,
            }
        })
        .collect();
    let checkout = checkout.map(|p| canon(&p.to_string_lossy()));
    let mut known_writers: Vec<String> = snap
        .runs
        .iter()
        .filter(|r| !bound.contains(r.id.as_str()) && r.ended_at_ms.is_none())
        .filter(|r| {
            matches!(
                r.execution.value,
                Execution::Working | Execution::Starting | Execution::RateLimited
            )
        })
        .filter(|r| {
            let cwd = r
                .cwd
                .clone()
                .or_else(|| snap.panes.get(&r.pane).cloned().flatten());
            match (&checkout, cwd) {
                (Some(co), Some(cwd)) => canon(&cwd).starts_with(co),
                _ => false,
            }
        })
        .map(|r| r.handle.clone())
        .collect();
    known_writers.sort();
    let mut open_interactions: Vec<String> = snap
        .interactions
        .iter()
        .filter(|i| i.status == InteractionStatus::Open && bound.contains(i.run.as_str()))
        .map(|i| i.id.clone())
        .collect();
    open_interactions.sort();
    let mut unresolved_deliveries: Vec<String> = snap
        .messages
        .iter()
        .filter(|m| {
            m.task == task_id
                && matches!(
                    m.state,
                    MessageState::Sending | MessageState::DeliveryUnknown
                )
        })
        .map(|m| m.id.clone())
        .collect();
    unresolved_deliveries.sort();
    let pending = active.iter().any(|b| snap.pending_runs.contains(&b.run_id));
    h.update(
        format!(
            "writers {known_writers:?}\ninteractions {open_interactions:?}\nsends {unresolved_deliveries:?}\npending {pending}\n"
        )
        .as_bytes(),
    );
    let mut n = [0u8; 8];
    n.copy_from_slice(&h.finalize().as_bytes()[..8]);
    let token = u64::from_le_bytes(n) & ((1u64 << 53) - 1);
    let live = LiveState {
        turn_finished: snap
            .runs
            .iter()
            .any(|r| bound.contains(r.id.as_str()) && r.turns_completed > 0),
        has_inspectable_changes: false,
        subject_is_current: false,
        sources_verified: false,
        bound_runs,
        known_writers,
        open_interactions,
        pending_binding_switch: pending,
        unresolved_deliveries,
        open_blocking_concerns: vec![],
        check_definition_digests: BTreeMap::new(),
        current_environment_digests: BTreeMap::new(),
        environment_unavailable: BTreeSet::new(),
        external_outcome_changed: false,
        acceptance: None,
    };
    (live, token)
}

/// A cheap token over everything acceptance must not silently miss (15 §7 "known competing
/// update"): intent revision, the task's bindings and pinned end candidates, its check runs
/// and their states, recorded acceptances and the live state. Computed with the core lock
/// held, both when a package build starts and inside the acceptance transaction.
fn state_token(c: &Core, task_id: &str) -> Option<u64> {
    let task = c
        .task(task_id)
        .cloned()
        .or_else(|| c.store.find::<Task>("task", task_id).ok().flatten())?;
    let bs = bindings_of(c, task_id);
    let mut h = blake3::Hasher::new();
    h.update(format!("intent {:?}\n", task.intent_revision).as_bytes());
    for b in &bs {
        h.update(format!("binding {} {:?} {:?}\n", b.id, b.state, b.end_turn).as_bytes());
    }
    let mut ends: Vec<EndRec> = by_task(c, K_END, task_id);
    ends.sort_by(|a, b| a.binding.cmp(&b.binding));
    for e in &ends {
        h.update(format!("end {} {:?}\n", e.binding, e.subject_id).as_bytes());
    }
    let mut runs: Vec<CheckRunRec> = by_task(c, K_CHECK, task_id);
    runs.sort_by(|a, b| a.run.id.cmp(&b.run.id));
    for r in &runs {
        h.update(
            format!(
                "check {} {:?} {}\n",
                r.run.id, r.run.state, r.run.subject_id
            )
            .as_bytes(),
        );
    }
    let mut accs: Vec<String> = acceptances_of(c, task_id)
        .into_iter()
        .map(|a| a.acceptance.id)
        .collect();
    accs.sort();
    h.update(format!("acceptances {accs:?}\n").as_bytes());
    h.update(t4::token_part(c, task_id).as_bytes());
    h.update(human::token_part(c, task_id).as_bytes());
    let snap = live_snap(c);
    let (_, live) = live_from(&snap, task_id, &bs, checkout_of(&task).as_deref());
    h.update(format!("live {live}\n").as_bytes());
    let mut n = [0u8; 8];
    n.copy_from_slice(&h.finalize().as_bytes()[..8]);
    Some(u64::from_le_bytes(n))
}

// ---- environment identity (§6.3, §7) ------------------------------------------------------------

/// Tools whose `--version` output identifies them (well-known toolchain binaries only; repo
/// files and unknown programs are never executed to learn their identity).
const VERSIONED_TOOLS: &[&str] = &[
    "cargo", "rustc", "node", "npm", "npx", "pnpm", "yarn", "bun", "deno", "python", "python3",
    "pip", "pip3", "go", "make", "gmake", "java", "mvn", "gradle", "ruby", "bundle", "uv",
    "pytest", "swift", "zig", "dotnet", "php", "composer", "cmake", "ninja", "gcc", "clang",
];

const SHELL_WORDS: &[&str] = &[
    "cd", "echo", "test", "[", "[[", "exit", "true", "false", "export", "set", "unset", "if",
    "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "until", "case", "esac", ".",
    "source", "exec", "env", "time", "command", "builtin", "!", "{", "}", "printf", "read",
    "shift", "return", "local", "trap", "wait", "eval",
];

/// Program names a check's command runs, in command position (argv[0]; first word of each
/// simple shell command), plus implied toolchain companions (cargo → rustc, npm → node).
fn command_tools(c: &CheckCommand) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut add = |w: &str| {
        let w = w.trim_matches(|ch| ch == '"' || ch == '\'');
        if w.is_empty() || SHELL_WORDS.contains(&w) || w.contains('=') || w.starts_with('$') {
            return;
        }
        out.insert(w.to_string());
    };
    match c {
        CheckCommand::Argv(v) => {
            if let Some(p) = v.first() {
                add(p);
            }
        }
        CheckCommand::Shell(s) => {
            add("sh");
            for seg in s.split([';', '|', '&', '\n', '(', ')', '`']) {
                let mut words = seg.split_whitespace();
                for w in words.by_ref() {
                    // Skip leading assignments and wrappers: `FOO=1 env time cargo test`.
                    let bare = w.trim_matches(|ch| ch == '"' || ch == '\'');
                    if bare.contains('=') || matches!(bare, "env" | "time" | "exec" | "command") {
                        continue;
                    }
                    add(w);
                    break;
                }
            }
        }
    }
    let implied: Vec<&str> = out
        .iter()
        .flat_map(|t| match t.as_str() {
            "cargo" => vec!["rustc"],
            "npm" | "npx" | "pnpm" | "yarn" => vec!["node"],
            _ => vec![],
        })
        .collect();
    let implied: Vec<String> = implied.into_iter().map(str::to_string).collect();
    out.extend(implied);
    out
}

/// Cached tool identities: (path, mtime, size, cwd) → (identity, probed at).
/// (tool path, mtime ns, size, cwd) → (version string, checked at).
type ToolCache = Mutex<HashMap<(PathBuf, i128, u64, PathBuf), (String, Instant)>>;

fn tool_cache() -> &'static ToolCache {
    static M: OnceLock<ToolCache> = OnceLock::new();
    M.get_or_init(Default::default)
}

const TOOL_CACHE_TTL: Duration = Duration::from_secs(60);

fn resolve_on_path(tool: &str, repo: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        // Relative PATH entries could resolve into the repository: never.
        if !dir.is_absolute() {
            continue;
        }
        let p = dir.join(tool);
        if let Ok(m) = std::fs::metadata(&p)
            && m.is_file()
            && std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 != 0
        {
            return Some(p);
        }
    }
    let _ = repo;
    None
}

fn file_stamp(p: &Path) -> Option<(i128, u64)> {
    let m = std::fs::metadata(p).ok()?;
    let mtime = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    Some((mtime, m.len()))
}

/// Identity of one tool: `--version` output for well-known toolchains (cached per
/// path+mtime+size+cwd for a minute), `path mtime size` for other programs on PATH,
/// `missing` when not found. Repo-relative programs belong to the subject and are skipped.
/// `Err` = identity unavailable (probe failed or timed out).
fn tool_identity(tool: &str, repo: &Path) -> Result<Option<String>, String> {
    let repo_c = canon(&repo.to_string_lossy());
    let path = if tool.contains('/') {
        let p = Path::new(tool);
        if !p.is_absolute() {
            return Ok(None);
        }
        p.to_path_buf()
    } else {
        match resolve_on_path(tool, repo) {
            Some(p) => p,
            None => return Ok(Some("missing".into())),
        }
    };
    let real = canon(&path.to_string_lossy());
    if real.starts_with(&repo_c) {
        return Ok(None);
    }
    let Some((mtime, size)) = file_stamp(&real) else {
        return Ok(Some("missing".into()));
    };
    let base = Path::new(tool)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !VERSIONED_TOOLS.contains(&base.as_str()) {
        return Ok(Some(format!(
            "{} mtime={mtime} size={size}",
            real.display()
        )));
    }
    let key = (real.clone(), mtime, size, repo_c.clone());
    if let Some((v, at)) = tool_cache().lock().unwrap().get(&key)
        && at.elapsed() < TOOL_CACHE_TTL
    {
        return Ok(Some(v.clone()));
    }
    let arg = if base == "go" { "version" } else { "--version" };
    let mut cmd = std::process::Command::new(&path);
    cmd.arg(arg)
        .current_dir(&repo_c)
        .env("RUSTUP_AUTO_INSTALL", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("{tool}: {e}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{tool} --version timed out"));
            }
        }
    };
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("{tool} {arg} exited with {status}"));
    }
    let text = String::from_utf8_lossy(if out.stdout.is_empty() {
        &out.stderr
    } else {
        &out.stdout
    })
    .lines()
    .next()
    .unwrap_or("")
    .trim()
    .to_string();
    if text.is_empty() {
        return Err(format!("{tool} {arg} printed nothing"));
    }
    tool_cache()
        .lock()
        .unwrap()
        .insert(key, (text.clone(), Instant::now()));
    Ok(Some(text))
}

/// Tool versions for a check's resolved command (recorded in the run's environment manifest
/// and compared for freshness). `Err` when any tool's identity is unavailable.
pub fn check_tools(def: &CheckDefinition, repo: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for t in command_tools(&def.command) {
        if let Some(v) = tool_identity(&t, repo)? {
            out.insert(t, v);
        }
    }
    Ok(out)
}

/// The environment digest a host run of `def` would record now (same hash as the run's
/// [`checks::EnvironmentManifest`]: os, arch, runner, tool versions).
pub fn current_environment(def: &CheckDefinition, repo: &Path) -> Result<String, String> {
    check_tools(def, repo).map(|t| checks::EnvironmentManifest::new("host", None, t, vec![]).digest)
}

// ---- authorization before retrieval (§11) -------------------------------------------------------

/// The workspace a pane-scoped caller may read (`None` = full scope).
fn caller_workspace(server: &Server, ctx: &Ctx) -> Result<Option<String>, RpcError> {
    let Some(pane) = &ctx.pane_scope else {
        return Ok(None);
    };
    server
        .with_core(|c| c.pane(pane).map(|p| p.workspace.clone()))
        .map(Some)
        .ok_or_else(|| {
            err(
                ErrorKind::PermissionDenied,
                "the caller's pane no longer exists",
            )
            .details(json!({"scope": "pane"}))
        })
}

/// Workspaces a task belongs to: its own `workspace` and those of the panes of runs bound to
/// it (default scope rule: workspace).
fn task_workspaces(c: &Core, task: &Task) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = task.workspace.iter().cloned().collect();
    for b in bindings_of(c, &task.id) {
        if let Some(ws) = c
            .run(&b.run_id)
            .and_then(|r| c.pane(&r.pane))
            .map(|p| p.workspace.clone())
        {
            out.insert(ws);
        }
    }
    out
}

/// Refuse before retrieving anything when a pane-scoped caller asks about a task outside its
/// pane's workspace.
fn authorize_task(server: &Server, ctx: &Ctx, task_id: &str) -> Result<(), RpcError> {
    let Some(ws) = caller_workspace(server, ctx)? else {
        return Ok(());
    };
    let task = tracking::find_task(server, task_id)?;
    if server.with_core(|c| task_workspaces(c, &task).contains(&ws)) {
        return Ok(());
    }
    Err(err(
        ErrorKind::PermissionDenied,
        "this task is outside the calling pane's workspace",
    )
    .details(json!({"scope": "pane", "reason": "outside_scope"})))
}

// ---- the review package (§6–§7) -----------------------------------------------------------------

/// Everything `task.review.get` computes, plus what to persist.
pub struct Pkg {
    pub task: Task,
    pub intent: Option<TaskIntent>,
    pub current: Option<ChangeSubject>,
    pub selected: Option<ChangeSubject>,
    pub live_subject: Option<ChangeSubject>,
    pub entries: Vec<CheckEntry>,
    pub assessment: ReviewAssessment,
    pub label: String,
    pub package_revision: u64,
    pub json: Value,
    /// Sources of the selected subject can be revalidated now (checkout present, immutable
    /// objects readable). `false` = historically inspectable only; no new acceptance.
    pub sources_verified: bool,
    /// [`state_token`] taken under the core lock when this build started.
    pub state_token: Option<u64>,
    /// Checkout and HEAD the current candidate was captured from, while a binding is live.
    pub live_head: Option<(PathBuf, String)>,
    /// Change digest a current dirty-snapshot candidate requires of the live checkout (T4).
    pub live_digest: Option<String>,
    subjects: Vec<ChangeSubject>,
    cand_recs: Vec<CandidateRec>,
    projection: Option<Projection>,
    invalidate: Option<AcceptanceRec>,
}

fn package_revision(
    intent_rev: Option<u32>,
    subject: Option<&str>,
    evidence: &[Evidence],
    digests: &BTreeMap<String, String>,
) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(format!("{intent_rev:?}|{subject:?}|").as_bytes());
    let mut ev: Vec<String> = evidence
        .iter()
        .map(|e| {
            format!(
                "{}:{:?}:{:?}:{:?}",
                e.id, e.outcome, e.definition_digest, e.subject_id
            )
        })
        .collect();
    ev.sort();
    for e in ev {
        h.update(e.as_bytes());
        h.update(b"\n");
    }
    for (k, v) in digests {
        h.update(format!("{k}={v}\n").as_bytes());
    }
    let b = h.finalize();
    let mut n = [0u8; 8];
    n.copy_from_slice(&b.as_bytes()[..8]);
    // JSON-safe integer.
    u64::from_le_bytes(n) & ((1u64 << 53) - 1)
}

fn task_check_runs(server: &Server, task: &str) -> Vec<CheckRunRec> {
    server.with_core(|c| {
        let mut v: Vec<CheckRunRec> = by_task(c, K_CHECK, task);
        v.sort_by(|a, b| a.run.id.cmp(&b.run.id));
        v.dedup_by(|a, b| a.run.id == b.run.id);
        v
    })
}

fn task_grants(server: &Server, task: &str) -> Vec<GrantRec> {
    server.with_core(|c| by_task(c, K_GRANT, task))
}

fn acceptance_history(server: &Server, task: &str) -> Vec<AcceptanceRec> {
    server.with_core(|c| {
        let mut v = acceptances_of(c, task);
        v.sort_by_key(|a| a.acceptance.accepted_at_ms);
        v.dedup_by(|a, b| a.acceptance.id == b.acceptance.id);
        v
    })
}

fn latest_acceptance(server: &Server, task: &str) -> Option<AcceptanceRec> {
    acceptance_history(server, task).pop()
}

fn subject_json(s: &ChangeSubject, current: bool, source: &str, extra: Value) -> Value {
    let mut v = json!({
        "subject": s,
        "id": s.id,
        "kind": s.kind,
        "head_sha": s.head_sha,
        "base_sha": s.base_sha,
        "current": current,
        "source": source,
        "accept_capable": s.is_immutable() && current,
    });
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, x) in e {
            o.insert(k.clone(), x.clone());
        }
    }
    v
}

/// Build the deterministic review package for `want_subject` (default: the current candidate).
/// Blocking (git); call off the state path.
pub fn build_package(
    server: &Server,
    task_id: &str,
    want_subject: Option<&str>,
) -> Result<Pkg, RpcError> {
    let task = tracking::find_task(server, task_id)?;
    // Taken before any input is read: anything acceptance-relevant that changes later makes
    // the acceptance transaction's token differ (15 §7).
    let state_token = server.with_core(|c| state_token(c, &task.id));
    let intent = task
        .intent_revision
        .and_then(|r| tracking::intent_at(server, &task.id, r));
    let bs = server.with_core(|c| bindings_of(c, &task.id));
    let cands = candidates_blocking(server, &task, &bs);

    // Every subject we know about for this task.
    let mut known: Vec<(ChangeSubject, &'static str, Option<String>)> = Vec::new();
    if let Some(cur) = &cands.current {
        let src = if cands.live {
            "current_head"
        } else {
            "binding_end"
        };
        known.push((cur.clone(), src, None));
    }
    for (e, s) in &cands.ends {
        if let Some(s) = s
            && !known.iter().any(|(k, _, _)| k.id == s.id)
        {
            known.push((s.clone(), "binding_end", Some(e.binding.clone())));
        }
    }
    for (s, src) in &cands.extra {
        if !known.iter().any(|(k, _, _)| k.id == s.id) {
            known.push((s.clone(), src, None));
        }
    }
    let selected = match want_subject {
        None => cands.current.clone(),
        Some(id) => {
            let found = known
                .iter()
                .map(|(s, _, _)| s)
                .chain(cands.live_subject.iter())
                .find(|s| s.id == id)
                .cloned();
            match found {
                Some(s) => Some(s),
                None => server
                    .with_core(|c| {
                        let assoc = c
                            .store
                            .get::<Value>(K_CAND, &format!("{}:{id}", task.id))
                            .ok()
                            .flatten()
                            .is_some();
                        assoc
                            .then(|| c.store.get::<ChangeSubject>(K_SUBJECT, id).ok().flatten())
                            .flatten()
                    })
                    .map(Some)
                    .ok_or_else(|| not_found("subject", id))?,
            }
        }
    };
    let subject_is_current = match (&selected, &cands.current) {
        (Some(a), Some(b)) => a.id == b.id,
        _ => false,
    };

    // Checks resolved at the selected candidate.
    let entries = selected.as_ref().map(resolve_checks).unwrap_or_default();
    let digests: BTreeMap<String, String> = entries
        .iter()
        .map(|e| (e.def.id.clone(), e.def.definition_digest.clone()))
        .collect();
    let runs = task_check_runs(server, &task.id);
    let grants = task_grants(server, &task.id);
    let grant_list: Vec<CheckGrant> = grants.iter().map(|g| g.grant.clone()).collect();

    // Evidence: Vibeke verifications, observed commands (unbound), claims.
    let mut evidence: Vec<Evidence> = runs
        .iter()
        .filter(|r| r.run.state.is_terminal())
        .map(|r| Evidence::from_check_run(&r.run))
        .collect();
    // Reviewer (role `review`) runs contribute notes, never evidence (T4).
    let (observed, claims) = observed_table(server, &t4::evidence_bindings(&bs));
    for cmd in &observed {
        let mapped = entries
            .iter()
            .find(|e| norm(&command_text(&e.def.command)) == norm(&cmd.command))
            .map(|e| (e.def.id.as_str(), e.def.definition_digest.as_str()));
        evidence.push(Evidence::from_observed(cmd, mapped));
    }
    for cl in &claims {
        evidence.push(Evidence::from_observed(cl, None));
    }
    // Screenshots (06 B6, §6.4): `browser` evidence, illustrative unless bound to the subject.
    let bound_runs: Vec<String> = bs.iter().map(|b| b.run_id.clone()).collect();
    let (shot_evidence, screenshots) = crate::screenshots::review_evidence(
        server,
        &task.id,
        &bound_runs,
        selected.as_ref(),
        intent.as_ref(),
    );
    evidence.extend(shot_evidence);
    // Recorded human reviews of human criteria (lane 2C).
    evidence.extend(human::evidence(server, &task.id));
    // Evidence whose content `forget` purged counts as unknown (15 §11, lane 2C).
    purge::degrade_purged(server, &task.id, &mut evidence);

    let snap = server.with_core(|c| live_snap(c));
    let (mut live, live_token) = live_from(&snap, &task.id, &bs, cands.checkout.as_deref());
    drop(snap);
    live.subject_is_current = subject_is_current;
    live.has_inspectable_changes =
        cands.current.is_some() || cands.live_subject.is_some() || !observed.is_empty();
    live.check_definition_digests = digests.clone();
    // Current environment identity per check (§7): unknown stays unknown, never "fresh".
    let mut env_errors: Vec<String> = Vec::new();
    if let Some(s) = &selected {
        let root = Path::new(&s.repo.root);
        for e in &entries {
            match current_environment(&e.def, root) {
                Ok(d) => {
                    live.current_environment_digests.insert(e.def.id.clone(), d);
                }
                Err(why) => {
                    live.environment_unavailable.insert(e.def.id.clone());
                    env_errors.push(format!(
                        "{}: environment identity unavailable ({why})",
                        e.def.id
                    ));
                }
            }
        }
    }
    // Historically inspectable vs currently verifiable (§7, §10.3): sources must be
    // revalidated now — the checkout exists and the subject's immutable objects are readable.
    let mut warnings = cands.warnings.clone();
    warnings.extend(env_errors);
    warnings.extend(patch::notes(selected.as_ref()));
    let diff_stat = match selected.as_ref().filter(|s| s.is_immutable()) {
        Some(s) => match subject::diff_stat(s) {
            Ok(d) => Some(d),
            Err(e) => {
                warnings.push(format!(
                    "Sources of revision {} unavailable: {e}",
                    short(&s.head_sha)
                ));
                None
            }
        },
        None => None,
    };
    let committed_selected = selected.as_ref().is_some_and(|s| s.is_immutable());
    let sources_verified = cands.checkout.is_some() && (!committed_selected || diff_stat.is_some());
    live.sources_verified = sources_verified;
    let acc = latest_acceptance(server, &task.id);
    live.acceptance = acc.as_ref().map(|a| a.acceptance.clone());
    // Reviewer findings (T4): unassessed potentially blocking notes and user-marked blockers on
    // the selected subject keep the stronger label away (15 §7).
    let notes = t4::notes_of(server, &task.id);
    live.open_blocking_concerns =
        t4::open_concerns(&notes, selected.as_ref().map(|s| s.id.as_str()));
    let effort_est = t4::effort_heuristic(
        diff_stat.as_ref(),
        &runs,
        selected.as_ref(),
        intent.as_ref(),
    );

    let defined: BTreeSet<&str> = entries.iter().map(|e| e.def.id.as_str()).collect();
    let mappings_confirmed = intent.as_ref().is_some_and(|i| {
        !i.criteria.is_empty()
            && i.criteria.iter().all(|c| {
                c.evaluation != Evaluation::Check
                    || (!c.check_definition_ids.is_empty()
                        && c.check_definition_ids
                            .iter()
                            .all(|d| defined.contains(d.as_str())))
            })
    });
    let mut assessment = readiness::assess(
        intent.as_ref(),
        selected.as_ref(),
        &evidence,
        &live,
        mappings_confirmed,
    );
    // Readiness without the acceptance (what a fresh review would see).
    let readiness_now = if acc.is_some() {
        let mut l2 = live.clone();
        l2.acceptance = None;
        readiness::assess(
            intent.as_ref(),
            selected.as_ref(),
            &evidence,
            &l2,
            mappings_confirmed,
        )
        .label
    } else {
        assessment.label
    };

    // Acceptance freshness, decided only against the current candidate (15 §7).
    let mut invalidate = None;
    let mut acc_outdated_reasons: Vec<String> = vec![];
    if let Some(a) = &acc {
        acc_outdated_reasons = a.outdated_reasons.clone();
        let viewing_current = want_subject.is_none() || subject_is_current;
        if a.outdated_at_ms.is_none()
            && viewing_current
            && let Some(intent) = &intent
        {
            let fs = FreshnessState::from_live(intent, selected.as_ref(), &live);
            let mut why: Vec<String> = readiness::outdated_reasons(&a.acceptance, &fs)
                .iter()
                .map(|r| r.text())
                .collect();
            if let Some(f) = runs.iter().find(|r| {
                r.run.subject_id == a.acceptance.subject_id
                    && r.run.state == CheckState::Failed
                    && r.run.ended_at_ms.unwrap_or(0) > a.acceptance.accepted_at_ms
            }) {
                why.push(format!(
                    "check {} failed on the accepted revision after acceptance",
                    f.run.check_id
                ));
            }
            if !why.is_empty() {
                acc_outdated_reasons = why.clone();
                invalidate = Some(AcceptanceRec {
                    acceptance: a.acceptance.clone(),
                    outdated_at_ms: Some(now()),
                    outdated_reasons: why,
                });
            }
        }
        let outdated = a.outdated_at_ms.is_some() || invalidate.is_some();
        if outdated && assessment.label != ReadinessLabel::ReviewOutdated {
            assessment.label = ReadinessLabel::ReviewOutdated;
            assessment.explanation.insert(
                0,
                format!("Review outdated: {}", acc_outdated_reasons.join("; ")),
            );
        }
    }
    let label = label_key(assessment.label, acc.as_ref().map(|a| &a.acceptance)).to_string();
    let pkg_rev = package_revision(
        intent.as_ref().map(|i| i.revision),
        selected.as_ref().map(|s| s.id.as_str()),
        &evidence,
        &digests,
    );

    // Candidate records to persist (first time a subject is a candidate of this task).
    let existing_cands: HashMap<String, i64> = server.with_core(|c| {
        known
            .iter()
            .filter_map(|(s, _, _)| {
                c.store
                    .get::<CandidateRec>(K_CAND, &format!("{}:{}", task.id, s.id))
                    .ok()
                    .flatten()
                    .map(|r| (s.id.clone(), r.created_at_ms))
            })
            .collect()
    });
    let cand_recs: Vec<CandidateRec> = known
        .iter()
        .filter(|(s, _, _)| !existing_cands.contains_key(&s.id))
        .map(|(s, src, b)| CandidateRec {
            task: task.id.clone(),
            subject_id: s.id.clone(),
            head_sha: s.head_sha.clone(),
            base_sha: s.base_sha.clone(),
            source: src.to_string(),
            binding: b.clone(),
            created_at_ms: now(),
        })
        .collect();
    let mut subjects: Vec<ChangeSubject> = known.iter().map(|(s, _, _)| s.clone()).collect();
    subjects.extend(cands.live_subject.clone());

    // Check entries for display, with authorization for the selected subject.
    let check_json: Vec<Value> = entries
        .iter()
        .map(|e| {
            let auth = selected
                .as_ref()
                .map(|s| checks::authorization_required(&e.def, s, &grant_list));
            let rs: Vec<&CheckRunRec> =
                runs.iter().filter(|r| r.run.check_id == e.def.id).collect();
            let on_subject: Vec<&CheckRun> = rs
                .iter()
                .filter(|r| selected.as_ref().is_some_and(|s| s.id == r.run.subject_id))
                .map(|r| &r.run)
                .collect();
            json!({
                "id": e.def.id,
                "name": e.def.name,
                "command": command_text(&e.def.command),
                "cwd": e.def.cwd,
                "timeout_ms": e.def.timeout_ms,
                "definition": e.def,
                "trust": e.def.trust,
                "provenance": e.provenance,
                "authorization": auth,
                "confirmation_label": checks::HOST_RUN_LABEL,
                "execution": {
                    "machine": server.opts.machine,
                    "runner": "host",
                    "checkout": "disposable checkout of the candidate commit",
                    "environment": "your environment with VIBEKE_* removed; no containment",
                    "side_effects": "Runs arbitrary code from this revision on this machine",
                },
                "runs_on_subject": on_subject,
                "latest_run": rs.last().map(|r| &r.run),
            })
        })
        .collect();

    // Candidate listings.
    let mut candidates: Vec<Value> = Vec::new();
    for (s, src, b) in &known {
        let is_cur = cands.current.as_ref().is_some_and(|c| c.id == s.id);
        let at = existing_cands.get(&s.id).copied();
        candidates.push(subject_json(
            s,
            is_cur,
            src,
            json!({"binding": b, "created_at_ms": at, "label": if is_cur { "Current candidate" } else { "Earlier candidate · inspect only" }}),
        ));
    }
    let inspect_only: Vec<Value> = cands
        .live_subject
        .iter()
        .map(|s| {
            subject_json(
                s,
                false,
                "live_checkout",
                json!({
                    "dirty_state": s.dirty_state,
                    "dirty_digest": s.dirty_digest,
                    "label": "Changes in this checkout · uncommitted · inspect only",
                    "limitations": ["Uncommitted work cannot be verified or accepted in T2", "May include changes not made by this task"],
                    "reason": "Select a committed revision to record acceptance",
                }),
            )
        })
        .collect();
    let no_end: Vec<Value> = cands
        .ends
        .iter()
        .filter(|(e, _)| e.subject_id.is_none())
        .map(|(e, _)| json!({"binding": e.binding, "run": e.run, "note": e.note.as_deref().unwrap_or("No bound end candidate"), "at_ms": e.at_ms}))
        .collect();

    let requires_exceptions: Vec<String> = assessment
        .criteria
        .iter()
        .filter(|a| a.required && a.needs_exception())
        .map(|a| a.criterion_id.clone())
        .collect();
    let accept_reason = if intent.is_none() {
        Some("Track this work to record acceptance")
    } else if selected.is_none() {
        Some(if cands.live_subject.is_some() {
            "Select a committed revision to record acceptance"
        } else {
            "No committed candidate to accept"
        })
    } else if selected.as_ref().is_some_and(|s| !s.is_immutable()) {
        Some("Select a committed revision to record acceptance")
    } else if !subject_is_current {
        Some("Only the current candidate can be accepted; earlier candidates are inspect only")
    } else if cands.checkout.is_none() {
        Some("Checkout unavailable: retained candidates are inspect only")
    } else if !sources_verified {
        Some("Sources of this revision cannot be verified now; acceptance is unavailable")
    } else {
        None
    };

    let baseline = tracking::baseline_of(server, &task.id)
        .and_then(|v| v.get("baseline").cloned())
        .unwrap_or(Value::Null);
    let mut live_json = serde_json::to_value(&live).unwrap_or(Value::Null);
    if let Some(o) = live_json.as_object_mut() {
        o.remove("acceptance");
    }
    let history: Vec<Value> = acceptance_history(server, &task.id)
        .into_iter()
        .map(|a| {
            json!({"acceptance": a.acceptance, "label": a.acceptance.label(), "status": if a.outdated_at_ms.is_some() { "outdated" } else { "current" }, "outdated_reasons": a.outdated_reasons})
        })
        .collect();
    let acceptance_json = acc.as_ref().map(|a| {
        let outdated = a.outdated_at_ms.is_some() || invalidate.is_some();
        json!({
            "acceptance": a.acceptance,
            "label": a.acceptance.label(),
            "status": if outdated { "outdated" } else { "current" },
            "outdated_reasons": acc_outdated_reasons,
        })
    });

    let mut json = json!({
        "task": task.id,
        "task_title": task.title,
        "package_revision": pkg_rev,
        "intent_revision": intent.as_ref().map(|i| i.revision),
        "intent": intent,
        "subject": selected,
        "subject_current": subject_is_current,
        "accept_capable": accept_reason.is_none(),
        "candidates": candidates,
        "inspect_only": inspect_only,
        "no_end_candidate": no_end,
        "review_base": cands.review_base,
        "baseline": baseline,
        "warnings": warnings,
        "diff_stat": diff_stat,
        "sources_verified": sources_verified,
        "historical_only": cands.checkout.is_none() || !sources_verified,
        "observed_commands": observed.iter().map(command_json).collect::<Vec<_>>(),
        "claims": claims.iter().map(command_json).collect::<Vec<_>>(),
        "screenshots": screenshots,
        "checks": check_json,
        "check_runs": runs.iter().map(|r| &r.run).collect::<Vec<_>>(),
        "assessment": assessment,
        "label": label,
        "label_text": label_text(&label),
        "readiness": {"label": label_key(readiness_now, None), "label_text": label_text(label_key(readiness_now, None))},
        "acceptance": acceptance_json,
        "acceptance_history": history,
        "live": live_json,
        "mappings_confirmed": mappings_confirmed,
        "review_notes": notes.iter().map(t4::note_json).collect::<Vec<_>>(),
        "reviewer_runs": t4::reviewer_requests(server, &task.id).iter().map(t4::reviewer_json).collect::<Vec<_>>(),
        // Full view; `task.review.get` re-filters it for pane-scoped callers.
        "dependencies": t4::dependencies_json(server, &task.id, None),
        "effort": {
            "set": task.effort,
            "heuristic": effort_est,
            "note": "Estimates are labelled with their source and never applied; set effort with task.set.",
        },
        "snapshot": {
            "available": cands.live && cands.dirty_state == Some(DirtyState::Dirty),
            "method": "task.review.snapshot",
            "note": "Capture the uncommitted work as an immutable, accept-capable subject (your files, index and branches stay as they are).",
        },
        "actions": {
            "accept": {
                "available": accept_reason.is_none(),
                "reason": accept_reason,
                "requires_exceptions": requires_exceptions,
            }
        },
    });

    // Lane 2C additions (kept out of the macro above: its recursion limit).
    json["human_reviews"] = json!(human::list_json(server, &task.id));
    json["purged"] = purge::package_json(server, &task.id);
    json["snapshot"]["selection"] = json!({
        "params": ["paths", "patch"],
        "note": "Pass paths (whole files) or patch (hunks) to capture only that selection on top of HEAD; checks on it verify the selection alone.",
    });

    let projection = (want_subject.is_none() || subject_is_current).then(|| {
        let mut failed: Vec<FailedCheck> = Vec::new();
        if let Some(cur) = &cands.current {
            let mut latest: BTreeMap<&str, &CheckRun> = BTreeMap::new();
            for r in runs
                .iter()
                .filter(|r| r.run.subject_id == cur.id && r.run.state.is_terminal())
            {
                latest.insert(r.run.check_id.as_str(), &r.run);
            }
            for (k, r) in latest {
                if matches!(
                    r.state,
                    CheckState::Failed | CheckState::Unknown | CheckState::Interrupted
                ) {
                    failed.push(FailedCheck {
                        run: r.id.clone(),
                        check: k.to_string(),
                        state: r.state,
                        ended_at_ms: r.ended_at_ms.unwrap_or(0),
                    });
                }
            }
        }
        Projection {
            task: task.id.clone(),
            label: label.clone(),
            label_text: label_text(&label).to_string(),
            subject_id: cands.current.as_ref().map(|s| s.id.clone()),
            head_sha: cands.current.as_ref().map(|s| s.head_sha.clone()),
            candidate_at_ms: cands
                .current
                .as_ref()
                .map(|s| existing_cands.get(&s.id).copied().unwrap_or_else(now)),
            intent_revision: intent.as_ref().map(|i| i.revision),
            package_revision: pkg_rev,
            revision: 0,
            failed_checks: failed,
            explanation: assessment.explanation.first().cloned(),
            updated_at_ms: now(),
            live_token: Some(live_token),
            effort_heuristic: (effort_est.effort != Effort::Unknown)
                .then(|| effort_str(effort_est.effort).to_string()),
        }
    });
    let live_head = (cands.live)
        .then(|| {
            cands
                .checkout
                .clone()
                .zip(cands.current.as_ref().map(|s| s.head_sha.clone()))
        })
        .flatten();
    let live_digest = (cands.live)
        .then(|| {
            cands
                .current
                .as_ref()
                .filter(|s| patch::digest_checked(s))
                .and_then(|s| s.dirty_digest.clone())
        })
        .flatten();

    Ok(Pkg {
        sources_verified,
        state_token,
        live_head,
        live_digest,
        task,
        intent,
        current: cands.current,
        selected,
        live_subject: cands.live_subject,
        entries,
        assessment,
        label,
        package_revision: pkg_rev,
        json,
        subjects,
        cand_recs,
        projection,
        invalidate,
    })
}

/// Persist what a package build discovered: subjects, candidates (`review.candidate_created`),
/// the projection, the task's review label (`review.label_changed`) and acceptance invalidation
/// (`review.invalidated`).
fn persist(server: &Server, pkg: &Pkg) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    for s in &pkg.subjects {
        if c.store
            .get::<Value>(K_SUBJECT, &s.id)
            .ok()
            .flatten()
            .is_none()
        {
            tx.m.close(K_SUBJECT, &s.id, None, s);
        }
    }
    for cr in &pkg.cand_recs {
        let key = format!("{}:{}", cr.task, cr.subject_id);
        if c.store.get::<Value>(K_CAND, &key).ok().flatten().is_none() {
            tx.m.close(K_CAND, &key, None, cr);
            tx.event(
                "review.candidate_created",
                json!({"task": cr.task}),
                json!({"subject": cr.subject_id, "head": cr.head_sha, "source": cr.source}),
            );
        }
    }
    if let Some(p) = &pkg.projection {
        let old = c
            .store
            .get::<Projection>(K_PROJ, &pkg.task.id)
            .ok()
            .flatten();
        let mut p = p.clone();
        let changed = old.as_ref().is_none_or(|o| {
            o.label != p.label
                || o.subject_id != p.subject_id
                || o.package_revision != p.package_revision
                || o.failed_checks != p.failed_checks
                || o.intent_revision != p.intent_revision
        });
        p.revision = old.as_ref().map_or(1, |o| o.revision + u64::from(changed));
        if changed
            || old
                .as_ref()
                .is_some_and(|o| o.explanation != p.explanation || o.live_token != p.live_token)
        {
            tx.m.put(K_PROJ, &pkg.task.id, None, &p);
        }
    }
    if pkg.projection.is_some() {
        let cur = c
            .task(&pkg.task.id)
            .cloned()
            .or_else(|| c.store.find::<Task>("task", &pkg.task.id).ok().flatten());
        if let Some(mut t) = cur {
            let keep_finished = t.review_label.as_deref() == Some("finished_without_review")
                && !pkg.label.starts_with("reviewed");
            if t.review_label.as_deref() != Some(pkg.label.as_str()) && !keep_finished {
                let from = t.review_label.clone();
                t.review_label = Some(pkg.label.clone());
                t.rev += 1;
                tx.task(t);
                tx.event(
                    "review.label_changed",
                    json!({"task": pkg.task.id}),
                    json!({"from": from, "to": pkg.label}),
                );
            }
        }
    }
    if let Some(a) = &pkg.invalidate {
        let fresh = c
            .store
            .get::<AcceptanceRec>(K_ACCEPT, &a.acceptance.id)
            .ok()
            .flatten();
        if fresh.is_some_and(|f| f.outdated_at_ms.is_none()) {
            tx.m.close(K_ACCEPT, &a.acceptance.id, None, a);
            tx.event(
                "review.invalidated",
                json!({"task": pkg.task.id, "acceptance": a.acceptance.id}),
                json!({"subject": a.acceptance.subject_id, "reasons": a.outdated_reasons}),
            );
        }
    }
    if !tx.m.is_empty() {
        let _ = server.commit(&mut c, tx);
    }
}

// ---- live-state invalidation of cached Ready (§7, §8) -------------------------------------------

const LIVE_DOWNGRADE_TEXT: &str = "Review available — agent/writer active or state unavailable";

/// Make sure a live-state watcher runs for this server: whenever the model changes it
/// compares each Ready task's cached live token with the current one and immediately
/// downgrades a stale Ready (turn started, question opened, uncertain send, another writer in
/// the checkout …), then queues a full refresh. One thread per server; it exits with the
/// server.
pub fn ensure_watcher(server: &Arc<Server>) {
    static W: OnceLock<Mutex<Vec<Weak<Server>>>> = OnceLock::new();
    let mut w = W.get_or_init(Default::default).lock().unwrap();
    w.retain(|x| x.strong_count() > 0);
    if w.iter()
        .any(|x| std::ptr::eq(x.as_ptr(), Arc::as_ptr(server)))
    {
        return;
    }
    w.push(Arc::downgrade(server));
    let weak = Arc::downgrade(server);
    // Event-driven (spec 10 §1.3 wakeup budget): block on the model revision instead of
    // polling it; the 50 ms nap after a change only coalesces bursts of commits. The thread
    // ends when the server (and with it the watch sender) is dropped.
    let mut rx = server.model_rev.subscribe();
    let _ = std::thread::Builder::new()
        .name("vk-review-live".into())
        .spawn(move || {
            let mut last = u64::MAX;
            loop {
                {
                    let Some(srv) = weak.upgrade() else { break };
                    let rev = *rx.borrow_and_update();
                    if rev != last {
                        last = rev;
                        downgrade_stale_ready(&srv);
                    }
                }
                if futures::executor::block_on(rx.changed()).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
}

/// Downgrade every task whose cached Ready label was computed under a different live state.
/// Cheap: model + store reads under one lock, no Git. Returns the downgraded task ids.
pub fn downgrade_stale_ready(server: &Arc<Server>) -> Vec<String> {
    let ready: Vec<(Task, Option<Projection>, Vec<TaskRunBinding>)> = server.with_core(|c| {
        c.model
            .tasks
            .iter()
            .filter(|t| t.review_label.as_deref() == Some("ready_for_review"))
            .map(|t| {
                (
                    t.clone(),
                    c.store.get::<Projection>(K_PROJ, &t.id).ok().flatten(),
                    bindings_of(c, &t.id),
                )
            })
            .collect()
    });
    if ready.is_empty() {
        return vec![];
    }
    let snap = server.with_core(|c| live_snap(c));
    let stale: Vec<(String, Option<u64>)> = ready
        .into_iter()
        .filter_map(|(t, p, bs)| {
            let (_, token) = live_from(&snap, &t.id, &bs, checkout_of(&t).as_deref());
            let cached = p.as_ref().and_then(|p| p.live_token);
            (cached != Some(token)).then_some((t.id, cached))
        })
        .collect();
    let mut out = Vec::new();
    if stale.is_empty() {
        return out;
    }
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        for (id, cached) in &stale {
            let Some(mut t) = c.task(id).cloned() else {
                continue;
            };
            if t.review_label.as_deref() != Some("ready_for_review") {
                continue;
            }
            let mut p = c
                .store
                .get::<Projection>(K_PROJ, id)
                .ok()
                .flatten()
                .unwrap_or_default();
            if p.live_token != *cached {
                // A refresh landed meanwhile; it is authoritative.
                continue;
            }
            if p.task.is_empty() {
                p.task = id.clone();
            }
            p.label = "review_available".into();
            p.label_text = label_text("review_available").into();
            p.explanation = Some(LIVE_DOWNGRADE_TEXT.into());
            p.revision += 1;
            p.updated_at_ms = now();
            tx.m.put(K_PROJ, id, None, &p);
            t.review_label = Some("review_available".into());
            t.rev += 1;
            tx.task(t);
            tx.event(
                "review.label_changed",
                json!({"task": id}),
                json!({"from": "ready_for_review", "to": "review_available", "reason": "live_state_changed"}),
            );
            out.push(id.clone());
        }
        if !tx.m.is_empty() {
            let _ = server.commit(&mut c, tx);
        }
    }
    for id in &out {
        spawn_refresh(server, id);
    }
    out
}

// ---- API ----------------------------------------------------------------------------------------

/// Accept the TUI's parameter spellings alongside the canonical ones: `subject_id` for
/// `subject`, `expected_intent_revision` / `expected_package_revision` on accept.
fn normalize(method: &str, p: &Value) -> Value {
    let mut p = p.clone();
    let Some(o) = p.as_object_mut() else { return p };
    if method.starts_with("task.check.")
        && !o.contains_key("subject")
        && let Some(v) = o.get("subject_id").cloned()
    {
        o.insert("subject".into(), v);
    }
    if method == "task.review.accept" {
        for (from, to) in [
            ("expected_intent_revision", "intent_revision"),
            ("expected_package_revision", "package_revision"),
        ] {
            if !o.contains_key(to)
                && let Some(v) = o.get(from).cloned()
            {
                o.insert(to.into(), v);
            }
        }
    }
    p
}

/// Flat aliases of the package for clients: `revision`, `criteria` (assessment rows merged with
/// the intent's text), `observed` (commands + claims with `category: "claim"`), `blockers`, and
/// `subject.accept_capable`.
fn with_aliases(mut j: Value) -> Value {
    let texts: std::collections::HashMap<String, String> = j["intent"]["criteria"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    Some((
                        c["id"].as_str()?.to_string(),
                        c["text"].as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let criteria: Vec<Value> = j["assessment"]["criteria"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| {
                    let mut c = c.clone();
                    let id = c["criterion_id"].as_str().unwrap_or("").to_string();
                    c["text"] = json!(texts.get(&id));
                    c["evidence"] = c["evidence_refs"].clone();
                    let st = c["status"].as_str().unwrap_or("");
                    let needs = match st {
                        "supported" => false,
                        "needs_judgment" => c["evaluation"] == "check",
                        _ => true,
                    };
                    c["needs_exception"] = json!(needs && c["required"] == true);
                    c
                })
                .collect()
        })
        .unwrap_or_default();
    let mut observed: Vec<Value> = j["observed_commands"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for mut c in j["claims"].as_array().cloned().unwrap_or_default() {
        c["category"] = json!("claim");
        if c["text"].is_null() {
            c["text"] = c["command"].clone();
        }
        observed.push(c);
    }
    j["revision"] = j["package_revision"].clone();
    j["criteria"] = json!(criteria);
    j["observed"] = json!(observed);
    j["blockers"] = j["assessment"]["blockers"].clone();
    let accept_capable = j["accept_capable"].clone();
    if j["subject"].is_object() {
        j["subject"]["accept_capable"] = accept_capable;
    }
    j
}

/// Test-only interleaving points (barrier-controlled concurrency tests).
#[cfg(test)]
pub(crate) mod hooks {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    pub type Hook = Arc<dyn Fn() + Send + Sync>;

    fn map() -> &'static Mutex<HashMap<String, Hook>> {
        static M: OnceLock<Mutex<HashMap<String, Hook>>> = OnceLock::new();
        M.get_or_init(Default::default)
    }

    pub fn set(point: &str, task: &str, f: Hook) {
        map().lock().unwrap().insert(format!("{point}:{task}"), f);
    }

    pub fn clear(point: &str, task: &str) {
        map().lock().unwrap().remove(&format!("{point}:{task}"));
    }

    pub fn fire(point: &str, task: &str) {
        let h = map()
            .lock()
            .unwrap()
            .get(&format!("{point}:{task}"))
            .cloned();
        if let Some(h) = h {
            h();
        }
    }
}

#[inline]
fn test_hook(_point: &str, _task: &str) {
    #[cfg(test)]
    hooks::fire(_point, _task);
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    let p = &normalize(method, p);
    Some(match method {
        "task.review.candidates" => review_candidates(server, ctx, p).await,
        "task.review.get" => review_get(server, ctx, p).await.map(with_aliases),
        "task.review.diff" => review_diff(server, ctx, p).await,
        "task.review.accept" => review_accept(server, ctx, p).await,
        "task.check.list" => check_list(server, ctx, p).await,
        "task.check.authorize" => check_authorize(server, ctx, p).await,
        // `authorize: true` = the explicit per-candidate authorization and the run in one user
        // action (still full scope only; the grant is recorded like task.check.authorize).
        "task.check.run" if p.get("authorize").and_then(Value::as_bool) == Some(true) => {
            let mut ap = p.clone();
            if let Some(k) = s(p, "idempotency_key") {
                ap["idempotency_key"] = json!(format!("{k}:authorize"));
            }
            if let Some(o) = ap.as_object_mut() {
                o.remove("authorize");
            }
            match check_authorize(server, ctx, &ap).await {
                Ok(_) => check_run(server, ctx, p).await,
                Err(e) => Err(e),
            }
        }
        "task.check.run" => check_run(server, ctx, p).await,
        "task.check.cancel" => check_cancel(server, ctx, p),
        "task.check.get" => check_get(server, ctx, p),
        m => return t4::api(server, ctx, m, p).await,
    })
}

async fn package(server: &Arc<Server>, task: &str, subject: Option<&str>) -> Result<Pkg, RpcError> {
    let srv = server.clone();
    let (task, subject) = (task.to_string(), subject.map(str::to_string));
    let pkg = blocking(move || {
        let pkg = build_package(&srv, &task, subject.as_deref())?;
        persist(&srv, &pkg);
        Ok::<_, RpcError>(pkg)
    })
    .await??;
    ensure_watcher(server);
    Ok(pkg)
}

/// `task.review.candidates {task}`.
async fn review_candidates(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let task = req(p, "task")?;
    authorize_task(server, ctx, task)?;
    let pkg = package(server, task, None).await?;
    let j = &pkg.json;
    Ok(json!({
        "task": pkg.task.id,
        "current": pkg.current.as_ref().map(|s| s.id.clone()),
        "candidates": j["candidates"],
        "inspect_only": j["inspect_only"],
        "no_end_candidate": j["no_end_candidate"],
        "review_base": j["review_base"],
        "warnings": j["warnings"],
    }))
}

/// `task.review.get {task, subject?}`: the deterministic package; no model call, no checks run.
async fn review_get(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let task = req(p, "task")?;
    authorize_task(server, ctx, task)?;
    let pkg = package(server, task, s(p, "subject")).await?;
    let mut j = pkg.json;
    // Linked tasks outside a pane-scoped caller's workspace are placeholders (15 §11).
    if let Some(ws) = caller_workspace(server, ctx)? {
        j["dependencies"] = t4::dependencies_json(server, &pkg.task.id, Some(&ws));
    }
    Ok(j)
}

/// `task.review.diff {task, subject?, path?, max_bytes?}`: the full diff of a committed
/// candidate from immutable Git objects (never the live tree), bounded to `max_bytes`
/// (default 256 KiB, at most 4 MiB). `subject` defaults to the current candidate.
async fn review_diff(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let task_id = req(p, "task")?.to_string();
    authorize_task(server, ctx, &task_id)?;
    let max = u(p, "max_bytes")
        .map(|m| (m as usize).clamp(1, DIFF_MAX_BYTES))
        .unwrap_or(DIFF_DEFAULT_BYTES);
    let path = s(p, "path").map(str::to_string);
    let want = s(p, "subject").map(str::to_string);
    let task = tracking::find_task(server, &task_id)?;
    // Fast path: a subject already recorded as a candidate of this task.
    let known = want.as_ref().and_then(|id| {
        server.with_core(|c| {
            let assoc = c
                .store
                .get::<Value>(K_CAND, &format!("{}:{id}", task.id))
                .ok()
                .flatten()
                .is_some();
            assoc
                .then(|| c.store.get::<ChangeSubject>(K_SUBJECT, id).ok().flatten())
                .flatten()
        })
    });
    let subj = match known {
        Some(s) => s,
        None => {
            let pkg = package(server, &task.id, want.as_deref()).await?;
            pkg.selected.ok_or_else(|| match &want {
                Some(id) => not_found("subject", id),
                None => conflict("no_subject", "this task has no committed candidate yet"),
            })?
        }
    };
    if !subj.is_immutable() {
        return Err(conflict(
            "subject_not_committed",
            "Uncommitted work has no immutable diff; inspect the live checkout or take a snapshot",
        ));
    }
    let s2 = subj.clone();
    let p2 = path.clone();
    let d = blocking(move || subject::diff_text_path(&s2, p2.as_deref(), max))
        .await?
        .map_err(|e| {
            conflict(
                "sources_unavailable",
                format!("the diff of this revision is unavailable: {e}"),
            )
        })?;
    Ok(json!({
        "task": task.id,
        "subject": subj.id,
        "base_sha": subj.base_sha,
        "head_sha": subj.head_sha,
        "content_sha": subj.content_sha(),
        "path": path,
        "diff": d.text,
        "truncated": d.truncated,
        "total_bytes": d.total_bytes,
        "max_bytes": max,
    }))
}

fn parse_exceptions(p: &Value) -> Result<Vec<CriterionException>, RpcError> {
    let Some(a) = p.get("exceptions") else {
        return Ok(vec![]);
    };
    let a = a
        .as_array()
        .ok_or_else(|| invalid("exceptions: [{criterion, reason}]"))?;
    a.iter()
        .map(|e| {
            Ok(CriterionException {
                criterion_id: s(e, "criterion")
                    .or_else(|| s(e, "criterion_id"))
                    .ok_or_else(|| invalid("exception needs `criterion`"))?
                    .to_string(),
                reason: s(e, "reason").unwrap_or("").to_string(),
            })
        })
        .collect()
}

fn review_changed(msg: impl Into<String>, details: Value) -> RpcError {
    let mut d = details;
    d["reason"] = json!("review_changed");
    err(ErrorKind::Conflict, msg).details(d)
}

/// `task.review.accept {task, intent_revision, subject_id, exceptions, package_revision?,
/// idempotency_key}` (15 §7). The package is rebuilt, the request validated against it, and
/// then — inside the transaction that records acceptance — the task's state token (intent
/// revision, bindings/end candidates, check runs and outcomes, acceptances, live blockers) is
/// recomputed and compared with the one the package was built under: any known competing
/// update returns `conflict` / `review_changed`.
async fn review_accept(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    const M: &str = "task.review.accept";
    if let Some(r) = receipts::replay(server, ctx, M, p) {
        return r;
    }
    let task_id = req(p, "task")?;
    let subject_id = req(p, "subject_id")?.to_string();
    let rev =
        u(p, "intent_revision").ok_or_else(|| invalid("missing param `intent_revision`"))? as u32;
    let exceptions = parse_exceptions(p)?;
    let pkg = package(server, task_id, None).await?;
    test_hook("accept_after_package", &pkg.task.id);
    let stored_live = server.with_core(|c| {
        c.store
            .get::<ChangeSubject>(K_SUBJECT, &subject_id)
            .ok()
            .flatten()
            .is_some_and(|s| !s.is_immutable())
    });
    if stored_live
        || pkg
            .live_subject
            .as_ref()
            .is_some_and(|l| l.id == subject_id)
    {
        return Err(conflict(
            "subject_not_committed",
            "Select a committed revision to record acceptance",
        ));
    }
    let intent = pkg
        .intent
        .clone()
        .ok_or_else(|| conflict("not_tracked", "task has no confirmed intent"))?;
    let areq = AcceptRequest {
        task_id: pkg.task.id.clone(),
        expected_intent_revision: rev,
        expected_package_revision: u(p, "package_revision").unwrap_or(pkg.package_revision),
        expected_subject_id: subject_id,
        exceptions,
        actor: tracking::user(ctx),
        idempotency_key: s(p, "idempotency_key").unwrap_or_default().to_string(),
    };
    // Unverifiable sources (checkout gone, objects unreadable) make retained candidates
    // inspect-only: `sources_unverified` (15 §4.3, §7).
    let snap = PackageSnapshot {
        task_id: &pkg.task.id,
        intent: &intent,
        package_revision: pkg.package_revision,
        subject: pkg.current.as_ref(),
        assessment: &pkg.assessment,
        sources_verified: pkg.sources_verified,
    };
    let acc = readiness::accept(&areq, &snap, now()).map_err(|e| {
        let mut d = serde_json::to_value(&e).unwrap_or(json!({}));
        d["reason"] = json!(e.reason());
        d["current_subject"] = json!(pkg.current.as_ref().map(|s| s.id.clone()));
        d["current_intent_revision"] = json!(intent.revision);
        d["package_revision"] = json!(pkg.package_revision);
        err(ErrorKind::Conflict, e.to_string()).details(d)
    })?;
    // Revalidate the source observation right before the transaction: a commit since the
    // package was captured is a known competing update.
    if let Some((path, head)) = pkg.live_head.clone() {
        let now_head = blocking(move || subject::rev_parse(&path, "HEAD").ok()).await?;
        if now_head.as_deref() != Some(head.as_str()) {
            return Err(review_changed(
                "the checkout moved to another revision during acceptance",
                json!({"field": "subject", "expected": head, "actual": now_head}),
            ));
        }
    }
    // A current dirty snapshot is current only while the checkout still holds exactly its
    // content (T4): a later edit is a known competing update.
    if let (Some((path, _)), Some(want)) = (pkg.live_head.clone(), pkg.live_digest.clone()) {
        let now_digest = blocking(move || {
            subject::observation_baseline(&path)
                .ok()
                .and_then(|b| b.change_digest)
        })
        .await?;
        if now_digest.as_deref() != Some(want.as_str()) {
            return Err(review_changed(
                "the uncommitted work changed since the snapshot; snapshot again and review",
                json!({"field": "subject", "detail": "snapshot_outdated"}),
            ));
        }
    }
    patch::revalidate(&pkg).await?;
    test_hook("accept_before_commit", &pkg.task.id);
    let mut c = server.core.lock().unwrap();
    // A concurrent duplicate of this request may have committed meanwhile.
    if let Some(r) = receipts::replay_in(&c, ctx, M, p) {
        return r;
    }
    // Serialize the expected-version checks with the write (15 §7).
    let cur_task = c
        .task(&pkg.task.id)
        .cloned()
        .ok_or_else(|| not_found("task", &pkg.task.id))?;
    if cur_task.intent_revision != Some(intent.revision) {
        return Err(review_changed(
            format!(
                "intent changed to revision {:?} during acceptance",
                cur_task.intent_revision
            ),
            json!({"field": "intent_revision", "expected": intent.revision, "actual": cur_task.intent_revision}),
        ));
    }
    let token_now = state_token(&c, &pkg.task.id);
    if pkg.state_token.is_none() || token_now != pkg.state_token {
        return Err(review_changed(
            "the review changed after it was shown (a check finished, a binding or candidate changed, another acceptance was recorded, or work resumed); refresh and review again",
            json!({"field": "state", "package_revision": pkg.package_revision}),
        ));
    }
    let label = if acc.with_exceptions() {
        "reviewed_with_exceptions"
    } else {
        "reviewed"
    };
    let mut t = cur_task;
    t.review_label = Some(label.into());
    t.rev += 1;
    let rec = AcceptanceRec {
        acceptance: acc.clone(),
        outdated_at_ms: None,
        outdated_reasons: vec![],
    };
    let mut tx = Tx::new();
    tx.m.close(K_ACCEPT, &acc.id, None, &rec);
    tx.task(t.clone());
    tx.event_by(
        "review.accepted",
        json!({"task": t.id, "acceptance": acc.id}),
        json!({"kind": "user", "id": acc.actor.id}),
        json!({"intent_revision": acc.intent_revision, "subject": acc.subject_id, "head": acc.head_sha, "exceptions": acc.exceptions.len(), "package_revision": acc.package_revision}),
    );
    let result = json!({
        "acceptance": acc,
        "label": label,
        "label_text": label_text(label),
        "task": t,
        "note": "Acceptance records this intent revision and revision only; it does not merge, finish or clean up.",
    });
    receipts::record(&mut tx, ctx, M, p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    spawn_refresh(server, &t.id);
    Ok(result)
}

/// `task.check.list {task, subject?}`.
async fn check_list(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let task = req(p, "task")?;
    authorize_task(server, ctx, task)?;
    let pkg = package(server, task, s(p, "subject")).await?;
    Ok(json!({
        "task": pkg.task.id,
        "subject": pkg.selected,
        "checks": pkg.json["checks"],
        "confirmation_label": checks::HOST_RUN_LABEL,
    }))
}

fn find_entry<'a>(pkg: &'a Pkg, check: &str) -> Result<&'a CheckEntry, RpcError> {
    pkg.entries
        .iter()
        .find(|e| e.def.id == check || e.def.name == check)
        .ok_or_else(|| not_found("check", check))
}

/// `task.check.authorize {task, check, subject, idempotency_key}`: the user's per-candidate
/// **Runs code modified by this task** action (15 §6.3). Never runs anything.
async fn check_authorize(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    const M: &str = "task.check.authorize";
    if let Some(r) = receipts::replay(server, ctx, M, p) {
        return r;
    }
    let subject_id = req(p, "subject")?;
    let pkg = package(server, req(p, "task")?, Some(subject_id)).await?;
    let subj = pkg
        .selected
        .clone()
        .ok_or_else(|| not_found("subject", subject_id))?;
    if !subj.is_immutable() {
        return Err(conflict(
            "verification_unbound",
            "Select a committed revision to verify",
        ));
    }
    let entry = find_entry(&pkg, req(p, "check")?)?;
    // The client confirmed a specific definition; a changed recipe needs a fresh review.
    if let Some(want) = s(p, "definition_digest")
        && want != entry.def.definition_digest
    {
        return Err(conflict("definition_changed", "the check definition changed since it was shown; review it again").details(json!({"reason": "definition_changed", "current": entry.def.definition_digest, "provenance": entry.provenance})));
    }
    let grant = checks::grant_per_candidate(&entry.def, &subj, tracking::user(ctx), now())
        .map_err(|e| err(ErrorKind::PermissionDenied, e.to_string()))?;
    let rec = GrantRec {
        task: pkg.task.id.clone(),
        grant: grant.clone(),
        definition: entry.def.clone(),
        head_sha: subj.head_sha.clone(),
    };
    let result = json!({
        "grant": grant,
        "definition": entry.def,
        "provenance": entry.provenance,
        "subject": subj.id,
        "head_sha": subj.head_sha,
        "confirmation_label": checks::HOST_RUN_LABEL,
        "execution": {"machine": server.opts.machine, "runner": "host", "checkout": "disposable checkout of the candidate commit"},
    });
    let mut c = server.core.lock().unwrap();
    if let Some(r) = receipts::replay_in(&c, ctx, M, p) {
        return r;
    }
    let mut tx = Tx::new();
    tx.m.close(K_GRANT, &grant.id, None, &rec);
    tx.event_by(
        "check.authorized",
        json!({"task": pkg.task.id, "check": entry.def.id}),
        json!({"kind": "user", "id": grant.authorized_by.id}),
        json!({"subject": subj.id, "definition_digest": entry.def.definition_digest, "trust": entry.def.trust}),
    );
    receipts::record(&mut tx, ctx, M, p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(result)
}

fn put_check(tx: &mut Tx, r: &CheckRunRec) {
    if r.run.state.is_terminal() {
        tx.m.close(K_CHECK, &r.run.id, None, r);
    } else {
        tx.m.put(K_CHECK, &r.run.id, None, r);
    }
}

/// `task.check.run {task, check, subject, idempotency_key}`: refuses without a matching
/// per-candidate grant (a changed definition needs fresh authorization); runs in a disposable
/// checkout on a background thread.
///
/// Idempotent per caller and key, also under concurrency: the run record and the caller's
/// receipt are inserted in one transaction after rechecking the receipt under the core lock,
/// so concurrent identical requests get the same run and exactly one execution starts.
async fn check_run(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    const M: &str = "task.check.run";
    if let Some(r) = receipts::replay(server, ctx, M, p) {
        return r;
    }
    let subject_id = req(p, "subject")?;
    let pkg = package(server, req(p, "task")?, Some(subject_id)).await?;
    let subj = pkg
        .selected
        .clone()
        .ok_or_else(|| not_found("subject", subject_id))?;
    let entry = find_entry(&pkg, req(p, "check")?)?;
    // The client confirmed a specific definition; a changed recipe needs a fresh review.
    if let Some(want) = s(p, "definition_digest")
        && want != entry.def.definition_digest
    {
        return Err(conflict("definition_changed", "the check definition changed since it was shown; review it again").details(json!({"reason": "definition_changed", "current": entry.def.definition_digest, "provenance": entry.provenance})));
    }
    let grants: Vec<CheckGrant> = task_grants(server, &pkg.task.id)
        .into_iter()
        .map(|g| g.grant)
        .collect();
    let grant = match checks::authorization_required(&entry.def, &subj, &grants) {
        AuthRequirement::Authorized { grant_id } => grants
            .iter()
            .find(|g| g.id == grant_id)
            .cloned()
            .ok_or_else(|| internal("grant vanished"))?,
        AuthRequirement::Unavailable { reason } => {
            return Err(conflict("verification_unbound", reason));
        }
        need @ AuthRequirement::Required { .. } => {
            return Err(err(
                ErrorKind::Conflict,
                "this check needs your per-candidate authorization before it runs",
            )
            .details(json!({
                "reason": "authorization_required",
                "requirement": need,
                "definition": entry.def,
                "provenance": entry.provenance,
                "confirmation_label": checks::HOST_RUN_LABEL,
            })));
        }
    };
    let key = s(p, "idempotency_key")
        .map(str::to_string)
        .unwrap_or_else(crate::core::ulid);
    let rec = CheckRunRec {
        task: pkg.task.id.clone(),
        run: CheckRun::queued(&entry.def, &subj, &grant, key),
        definition: entry.def.clone(),
        subject: subj.clone(),
    };
    let result =
        json!({"check_run": rec.run, "definition": rec.definition, "provenance": entry.provenance});
    test_hook("check_run_before_reserve", &pkg.task.id);
    {
        let mut c = server.core.lock().unwrap();
        // Transactional reservation: whoever commits first owns the key; everyone else
        // replays that run.
        if let Some(r) = receipts::replay_in(&c, ctx, M, p) {
            return r;
        }
        let mut tx = Tx::new();
        put_check(&mut tx, &rec);
        tx.event(
            "check.queued",
            json!({"task": rec.task, "check": rec.run.check_id, "check_run": rec.run.id}),
            json!({"subject": subj.id, "head": subj.head_sha}),
        );
        receipts::record(&mut tx, ctx, M, p, &result);
        server.commit(&mut c, tx).map_err(internal)?;
    }
    let token = checks::CancelToken::new();
    cancels()
        .lock()
        .unwrap()
        .insert(rec.run.id.clone(), token.clone());
    let srv = server.clone();
    std::thread::spawn(move || execute(&srv, rec, grant, token));
    Ok(result)
}

fn execute(
    server: &Arc<Server>,
    mut rec: CheckRunRec,
    grant: CheckGrant,
    token: checks::CancelToken,
) {
    if !token.is_cancelled() {
        rec.run.state = CheckState::Running;
        rec.run.started_at_ms = Some(now());
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        put_check(&mut tx, &rec);
        tx.event(
            "check.started",
            json!({"task": rec.task, "check": rec.run.check_id, "check_run": rec.run.id}),
            json!({}),
        );
        let _ = server.commit(&mut c, tx);
    }
    // The run's environment manifest carries the same tool identities freshness compares
    // against (§6.3, §7). If some identity is unavailable, the current side is unavailable too.
    let tools = check_tools(&rec.definition, Path::new(&rec.subject.repo.root)).unwrap_or_default();
    let opts = checks::RunOptions {
        idempotency_key: rec.run.idempotency_key.clone(),
        tool_versions: tools,
        ..Default::default()
    };
    let out = checks::run_in_disposable_checkout(
        Path::new(&rec.subject.repo.root),
        &rec.subject,
        &rec.definition,
        &grant,
        &server.paths.state.join("checks"),
        &token,
        &opts,
    );
    cancels().lock().unwrap().remove(&rec.run.id);
    match out {
        Ok(mut r) => {
            r.id = rec.run.id.clone();
            r.idempotency_key = rec.run.idempotency_key.clone();
            if r.started_at_ms.is_none() {
                r.started_at_ms = rec.run.started_at_ms;
            }
            rec.run = r;
        }
        Err(e) => {
            // Nothing ran (refused before start): record it without inventing a result.
            rec.run.state = if rec.run.state == CheckState::Queued && token.is_cancelled() {
                CheckState::Cancelled
            } else {
                CheckState::Interrupted
            };
            rec.run.error = Some(e.to_string());
            rec.run.ended_at_ms = Some(now());
        }
    }
    let kind = format!(
        "check.{}",
        serde_json::to_value(rec.run.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".into())
    );
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        put_check(&mut tx, &rec);
        tx.event(
            &kind,
            json!({"task": rec.task, "check": rec.run.check_id, "check_run": rec.run.id}),
            json!({"subject": rec.run.subject_id, "exit_code": rec.run.exit_code, "error": rec.run.error}),
        );
        let _ = server.commit(&mut c, tx);
    }
    spawn_refresh(server, &rec.task);
}

fn load_check(server: &Server, id: &str) -> Result<CheckRunRec, RpcError> {
    server
        .with_core(|c| c.store.get::<CheckRunRec>(K_CHECK, id).ok().flatten())
        .ok_or_else(|| not_found("check_run", id))
}

/// `task.check.cancel {check_run}`: cooperative; partial output kept, never a fabricated result.
fn check_cancel(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let id = req(p, "check_run")?;
    let rec = load_check(server, id)?;
    authorize_task(server, ctx, &rec.task)?;
    if rec.run.state.is_terminal() {
        return Ok(json!({"check_run": rec.run, "cancelling": false}));
    }
    if let Some(t) = cancels().lock().unwrap().get(id) {
        t.cancel();
        return Ok(json!({"check_run": rec.run, "cancelling": true}));
    }
    // Not running in this process (e.g. after a restart): reconcile without relaunching.
    let mut rec = rec;
    rec.run.state = rec.run.state_after_unreconciled_restart();
    rec.run.error = Some("runner not found in this server; outcome not reconciled".into());
    rec.run.ended_at_ms = Some(now());
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    put_check(&mut tx, &rec);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(json!({"check_run": rec.run, "cancelling": false}))
}

/// `task.check.get {check_run}`: the run, its definition and the log tail.
/// Authorized before the log is read (15 §11).
fn check_get(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let rec = load_check(server, req(p, "check_run")?)?;
    authorize_task(server, ctx, &rec.task)?;
    let tail = rec.run.log_path.as_ref().and_then(|lp| {
        let b = std::fs::read(lp).ok()?;
        let start = b.len().saturating_sub(8192);
        Some(String::from_utf8_lossy(&b[start..]).into_owned())
    });
    Ok(
        json!({"check_run": rec.run, "definition": rec.definition, "task": rec.task, "log_tail": tail}),
    )
}

/// Startup: queued/running checks from a previous server can't be reconciled with a runner,
/// so queued → interrupted and running → unknown. Never relaunched (15 §6.3).
pub fn recover(server: &Arc<Server>) {
    ensure_watcher(server);
    attention_ext::start(server);
    scratch::recover(server);
    t4::recover(server);
    let stale: Vec<CheckRunRec> =
        server.with_core(|c| c.store.load::<CheckRunRec>(K_CHECK).unwrap_or_default());
    if stale.is_empty() {
        return;
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    for mut r in stale {
        if r.run.state.is_terminal() {
            put_check(&mut tx, &r);
            continue;
        }
        r.run.state = r.run.state_after_unreconciled_restart();
        r.run.error = Some("the server restarted; outcome not reconciled".into());
        r.run.ended_at_ms = Some(now());
        put_check(&mut tx, &r);
        let kind = if r.run.state == CheckState::Unknown {
            "check.unknown"
        } else {
            "check.interrupted"
        };
        tx.event(
            kind,
            json!({"task": r.task, "check": r.run.check_id, "check_run": r.run.id}),
            json!({"reason": "server_restarted"}),
        );
    }
    let _ = server.commit(&mut c, tx);
}

// ---- attention (T3, §8) -------------------------------------------------------------------------

struct Meta {
    kind: &'static str,
    id: String,
    title: String,
    subtitle: String,
    task: Option<String>,
    run: Option<String>,
    pane: Option<String>,
    interaction: Option<String>,
    snoozed_until_ms: Option<i64>,
    woke: Option<String>,
}

fn effort_of(e: Option<&str>) -> Effort {
    match e {
        Some("quick") => Effort::Quick,
        Some("minutes") => Effort::FewMinutes,
        Some("deep") => Effort::DeepReview,
        _ => Effort::Unknown,
    }
}

fn effort_str(e: Effort) -> &'static str {
    match e {
        Effort::Quick => "quick",
        Effort::FewMinutes => "minutes",
        Effort::DeepReview => "deep",
        Effort::Unknown => "unknown",
    }
}

fn risk_of(r: &Risk) -> att::Risk {
    match r {
        Risk::Low => att::Risk::Low,
        Risk::Medium => att::Risk::Medium,
        Risk::High => att::Risk::High,
        Risk::Unknown => att::Risk::Unknown,
    }
}

fn risk_str(r: att::Risk) -> &'static str {
    match r {
        att::Risk::Low => "low",
        att::Risk::Medium => "medium",
        att::Risk::High => "high",
        att::Risk::Unknown => "unknown",
    }
}

const REVIEW_LABELS: &[&str] = &[
    "review_available",
    "ready_for_review",
    "changes_to_inspect",
    "review_outdated",
    "needs_task_details",
];

struct Collected {
    items: Vec<AttentionItem>,
    meta: HashMap<String, Meta>,
    notes: Vec<String>,
    complete: bool,
    stale_tasks: Vec<String>,
    /// Workspaces each item belongs to (object id → workspaces), for scoped callers.
    item_ws: HashMap<String, BTreeSet<String>>,
}

/// Gather attention items from the model and cached projections (no git, one lock).
fn collect(server: &Server, now_ms: i64) -> Collected {
    let machine = server.opts.machine.clone();
    let inflight_cutoff = now_ms - 24 * 3600 * 1000;
    let (
        ints,
        runs,
        tasks,
        reads,
        open_bindings,
        projections,
        messages,
        prefs,
        closed_ints,
        pane_ws,
        snap,
    ) = server.with_core(|c| {
        (
            c.model.interactions.clone(),
            c.model.runs.clone(),
            c.model.tasks.clone(),
            c.store.reads("local").unwrap_or_default(),
            c.store
                .load::<TaskRunBinding>(tracking::K_BINDING)
                .unwrap_or_default(),
            c.store.load::<Projection>(K_PROJ).unwrap_or_default(),
            c.store
                .load::<tracking::TaskMessage>(tracking::K_MESSAGE)
                .unwrap_or_default(),
            c.store.load::<Pref>(K_PREF).unwrap_or_default(),
            c.store
                .load_closed::<Interaction>("interaction", 200)
                .unwrap_or_default(),
            c.model
                .panes
                .iter()
                .map(|p| (p.id.clone(), p.workspace.clone()))
                .collect::<HashMap<String, String>>(),
            live_snap(c),
        )
    });
    let tasks_by_id: HashMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let runs_by_id: HashMap<&str, &AgentRun> = runs.iter().map(|r| (r.id.as_str(), r)).collect();
    let proj_by_task: HashMap<&str, &Projection> =
        projections.iter().map(|p| (p.task.as_str(), p)).collect();
    let seen: HashMap<&str, u64> = reads.iter().map(|(p, r)| (p.as_str(), *r)).collect();
    let mut items = Vec::new();
    let mut meta = HashMap::new();
    let mut notes = Vec::new();
    let mut stale_tasks = Vec::new();

    let who = |r: Option<&&AgentRun>| {
        r.map(|r| r.name.clone().unwrap_or_else(|| r.harness.clone()))
            .unwrap_or_else(|| "agent".into())
    };
    let task_for_run = |run: &AgentRun| {
        vk_review::binding::binding_for_turn(&open_bindings, &run.id, run.turns_completed + 1)
            .filter(|b| b.state == BindingState::Active)
            .map(|b| b.task_id.clone())
    };
    let mut push = |item: AttentionItem, m: Meta| {
        meta.insert(item.key.object_id.clone(), m);
        items.push(item);
    };
    let base = |object_id: String, revision: u64, kind: AttentionKind, title: &str, opened: i64| {
        AttentionItem {
            key: ItemKey {
                object_id,
                revision,
            },
            kind,
            task_id: None,
            run_id: None,
            machine: machine.clone(),
            title: title.to_string(),
            opened_at_ms: opened,
            deadline_ms: None,
            priority: 0,
            risk: None,
            blocks_run: false,
            blocks_tasks: 0,
            effort: Effort::Unknown,
            seen: false,
            snoozed_until_ms: None,
            pinned: false,
            unsafe_source: false,
            interaction: None,
        }
    };

    // Open interactions (with native deadlines and batch facts, lane 2C).
    let afacts = attention_ext::facts(server);
    for i in ints.iter().filter(|i| i.status == InteractionStatus::Open) {
        let run = runs_by_id.get(i.run.as_str());
        let task_id = run.and_then(|r| task_for_run(r));
        let task = task_id.as_deref().and_then(|t| tasks_by_id.get(t));
        let mut it = base(
            format!("interaction:{}", i.id),
            i.decision_rev as u64,
            AttentionKind::Interaction,
            &i.title,
            i.opened_at_ms,
        );
        it.task_id = task_id.clone();
        it.run_id = Some(i.run.clone());
        it.priority = task.and_then(|t| t.priority).unwrap_or(0);
        it.risk = i.action.as_ref().map(|a| risk_of(&a.risk));
        it.blocks_run = i.kind != InteractionKind::Notice;
        it.effort = effort_of(task.and_then(|t| t.effort.as_deref()));
        it.interaction = Some(att::InteractionFacts {
            kind: match i.kind {
                InteractionKind::Approval => att::InteractionKind::Approval,
                InteractionKind::Question => att::InteractionKind::Question,
                InteractionKind::PlanReview => att::InteractionKind::PlanReview,
                InteractionKind::Notice => att::InteractionKind::Notice,
            },
            live: true,
            native_answer: i.answerable,
            approval: None,
        });
        attention_ext::decorate_item(&afacts, &i.id, &mut it);
        let mut sub = vec![who(run), i.kind.as_str().replace('_', " ")];
        if let Some(t) = task {
            sub.push(t.title.clone());
        }
        push(
            it,
            Meta {
                kind: "interaction",
                id: i.id.clone(),
                title: i.title.clone(),
                subtitle: sub.join(" · "),
                task: task_id,
                run: Some(i.run.clone()),
                pane: Some(i.pane.clone()),
                interaction: Some(i.id.clone()),
                snoozed_until_ms: None,
                woke: None,
            },
        );
    }

    // Decision deliveries whose outcome is unknown or failed (class 1).
    for i in closed_ints.iter().filter(|i| {
        matches!(
            i.delivery,
            DeliveryState::DeliveryUnknown | DeliveryState::Failed
        ) && i.answered_at_ms.unwrap_or(i.opened_at_ms) >= inflight_cutoff
    }) {
        let run = runs_by_id.get(i.run.as_str());
        let mut it = base(
            format!("send_unknown:{}", i.id),
            i.decision_rev as u64,
            AttentionKind::DeliveryProblem,
            &i.title,
            i.answered_at_ms.unwrap_or(i.opened_at_ms),
        );
        it.run_id = Some(i.run.clone());
        push(
            it,
            Meta {
                kind: "send_unknown",
                id: i.id.clone(),
                title: format!("Answer delivery unconfirmed: {}", i.title),
                subtitle: format!(
                    "{} · {}",
                    who(run),
                    i.delivery_error.as_deref().unwrap_or("check the pane")
                ),
                task: None,
                run: Some(i.run.clone()),
                pane: Some(i.pane.clone()),
                interaction: Some(i.id.clone()),
                snoozed_until_ms: None,
                woke: None,
            },
        );
    }
    // Task messages with an unknown delivery outcome (class 1).
    for m in messages
        .iter()
        .filter(|m| m.state == MessageState::DeliveryUnknown)
    {
        let task = tasks_by_id.get(m.task.as_str());
        let mut it = base(
            format!("send_unknown:{}", m.id),
            m.updated_at_ms.max(0) as u64,
            AttentionKind::DeliveryProblem,
            "Message delivery unconfirmed",
            m.updated_at_ms,
        );
        it.task_id = Some(m.task.clone());
        it.run_id = Some(m.run.clone());
        it.priority = task.and_then(|t| t.priority).unwrap_or(0);
        let preview: String = m.text.chars().take(60).collect();
        push(
            it,
            Meta {
                kind: "send_unknown",
                id: m.id.clone(),
                title: "Message delivery unconfirmed".into(),
                subtitle: format!(
                    "{} · \"{preview}\"",
                    task.map(|t| t.title.as_str()).unwrap_or("task")
                ),
                task: Some(m.task.clone()),
                run: Some(m.run.clone()),
                pane: runs_by_id.get(m.run.as_str()).map(|r| r.pane.clone()),
                interaction: None,
                snoozed_until_ms: None,
                woke: None,
            },
        );
    }

    // Tracked tasks: review candidates and failed checks, from cached projections.
    for t in tasks
        .iter()
        .filter(|t| t.intent_revision.is_some() && matches!(t.status.as_str(), "active" | "parked"))
    {
        let Some(p) = proj_by_task.get(t.id.as_str()) else {
            stale_tasks.push(t.id.clone());
            continue;
        };
        // A cached Ready is only shown while the live state it was computed under holds
        // (turn started, question opened, uncertain send, another writer → not Ready).
        let mut label_text_now = p.label_text.clone();
        if p.label == "ready_for_review" {
            let bs: Vec<TaskRunBinding> = open_bindings
                .iter()
                .filter(|b| b.task_id == t.id)
                .cloned()
                .collect();
            let (_, token) = live_from(&snap, &t.id, &bs, checkout_of(t).as_deref());
            if p.live_token != Some(token) {
                label_text_now = LIVE_DOWNGRADE_TEXT.into();
                stale_tasks.push(t.id.clone());
            }
        }
        if let Some(sid) = &p.subject_id
            && REVIEW_LABELS.contains(&p.label.as_str())
        {
            let id = format!("{}:{sid}", t.id);
            let mut it = base(
                format!("review:{id}"),
                p.revision,
                AttentionKind::ReviewCandidate,
                &t.title,
                p.candidate_at_ms.unwrap_or(p.updated_at_ms),
            );
            it.task_id = Some(t.id.clone());
            it.priority = t.priority.unwrap_or(0);
            it.effort = effort_of(t.effort.as_deref());
            let mut sub = vec![label_text_now.clone()];
            if let Some(h) = &p.head_sha {
                sub.push(format!("revision {}", short(h)));
            }
            if !p.failed_checks.is_empty() {
                sub.push(format!("{} check(s) failed", p.failed_checks.len()));
            }
            push(
                it,
                Meta {
                    kind: "review",
                    id,
                    title: t.title.clone(),
                    subtitle: sub.join(" · "),
                    task: Some(t.id.clone()),
                    run: None,
                    pane: None,
                    interaction: None,
                    snoozed_until_ms: None,
                    woke: None,
                },
            );
        }
        for f in &p.failed_checks {
            let mut it = base(
                format!("check_failed:{}", f.run),
                1,
                AttentionKind::Error,
                &format!("{} failed", f.check),
                f.ended_at_ms,
            );
            it.task_id = Some(t.id.clone());
            it.priority = t.priority.unwrap_or(0);
            it.effort = effort_of(t.effort.as_deref());
            // A lost runner leaves an uncertain outcome (class 1); a test failure stays with
            // ordinary blocking decisions.
            it.unsafe_source = f.state != CheckState::Failed;
            let what = match f.state {
                CheckState::Failed => "failed",
                CheckState::Interrupted => "was interrupted",
                _ => "has an unknown outcome",
            };
            push(
                it,
                Meta {
                    kind: "check_failed",
                    id: f.run.clone(),
                    title: format!("Check {} {what}", f.check),
                    subtitle: format!(
                        "{}{}",
                        t.title,
                        p.head_sha
                            .as_deref()
                            .map(|h| format!(" · revision {}", short(h)))
                            .unwrap_or_default()
                    ),
                    task: Some(t.id.clone()),
                    run: None,
                    pane: None,
                    interaction: None,
                    snoozed_until_ms: None,
                    woke: None,
                },
            );
        }
    }

    // Suspended bindings waiting for Continue task / Track new work.
    for b in open_bindings.iter().filter(|b| {
        b.state == BindingState::Suspended
            && !open_bindings
                .iter()
                .any(|x| x.task_id == b.task_id && x.state == BindingState::Active)
    }) {
        let Some(t) = tasks_by_id.get(b.task_id.as_str()) else {
            continue;
        };
        if !matches!(t.status.as_str(), "active" | "parked") {
            continue;
        }
        let mut it = base(
            format!("binding_suspended:{}", b.id),
            1,
            AttentionKind::Error,
            "Conversation changed",
            b.created_at_ms,
        );
        it.task_id = Some(t.id.clone());
        it.run_id = Some(b.run_id.clone());
        it.priority = t.priority.unwrap_or(0);
        push(
            it,
            Meta {
                kind: "binding_suspended",
                id: b.id.clone(),
                title: "Conversation changed — continue task or track new work?".into(),
                subtitle: t.title.clone(),
                task: Some(t.id.clone()),
                run: Some(b.run_id.clone()),
                pane: runs_by_id.get(b.run_id.as_str()).map(|r| r.pane.clone()),
                interaction: None,
                snoozed_until_ms: None,
                woke: None,
            },
        );
    }

    // Finished, unseen turns (the M1 next_attention fallback), tracked or not.
    for r in runs.iter().filter(|r| {
        r.execution.value == Execution::Idle
            && r.done_rev > seen.get(r.pane.as_str()).copied().unwrap_or(0)
    }) {
        let mut it = base(
            format!("finished_turn:{}", r.id),
            r.done_rev,
            AttentionKind::FinishedTurn,
            "Turn finished",
            r.execution.since_ms,
        );
        it.run_id = Some(r.id.clone());
        it.task_id = task_for_run(r);
        let last: Option<String> = r
            .last_message
            .as_ref()
            .map(|m| m.chars().take(80).collect());
        push(
            it,
            Meta {
                kind: "finished_turn",
                id: r.id.clone(),
                title: format!("{} finished a turn", who(Some(&r))),
                subtitle: last.unwrap_or_else(|| "✓ done".into()),
                task: task_for_run(r),
                run: Some(r.id.clone()),
                pane: Some(r.pane.clone()),
                interaction: None,
                snoozed_until_ms: None,
                woke: None,
            },
        );
    }

    // Confirmed dependency links (T4, §8.1): open tasks waiting for this item's task.
    let blocked = server.with_core(|c| t4::blocked_counts(c));
    if !blocked.is_empty() {
        for it in items.iter_mut() {
            if let Some(n) = it.task_id.as_ref().and_then(|t| blocked.get(t)) {
                it.blocks_tasks = *n as u32;
            }
        }
    }

    // Per-user preferences: seen, pin, snooze with material wake-ups.
    let aprefs = attention_ext::prefs();
    let prefs: HashMap<&str, &Pref> = prefs.iter().map(|p| (p.key.as_str(), p)).collect();
    for it in items.iter_mut() {
        let Some(pf) = prefs.get(it.key.object_id.as_str()) else {
            continue;
        };
        it.pinned = pf.pinned;
        it.seen = pf.seen_rev.is_some_and(|r| r >= it.key.revision);
        if let Some(until) = pf.snoozed_until_ms.filter(|u| *u > now_ms) {
            it.snoozed_until_ms = Some(until);
            let m = meta.get_mut(&it.key.object_id);
            if let Some(m) = m {
                m.snoozed_until_ms = Some(until);
                if let Some(snap) = &pf.snapshot
                    && let Some(w) = att::wake(it, snap, now_ms, &aprefs)
                {
                    // Woken: clients must not keep hiding it behind the old deadline.
                    m.woke = Some(w.text(now_ms));
                    m.snoozed_until_ms = None;
                    it.snoozed_until_ms = None;
                }
            }
        }
    }

    let complete = stale_tasks.is_empty();
    if !complete {
        notes.push(format!(
            "{} tracked task(s) have no current review projection yet; refreshing",
            stale_tasks.len()
        ));
    }
    // Scope of each item: its task's workspace(s) (own workspace + panes of bound runs), and
    // the workspace of its pane/run.
    let task_ws = |tid: &str| -> BTreeSet<String> {
        let mut out: BTreeSet<String> = tasks_by_id
            .get(tid)
            .and_then(|t| t.workspace.clone())
            .into_iter()
            .collect();
        for b in open_bindings.iter().filter(|b| b.task_id == tid) {
            if let Some(ws) = runs_by_id
                .get(b.run_id.as_str())
                .and_then(|r| pane_ws.get(&r.pane))
            {
                out.insert(ws.clone());
            }
        }
        out
    };
    let item_ws: HashMap<String, BTreeSet<String>> = meta
        .iter()
        .map(|(k, m)| {
            let mut ws = m.task.as_deref().map(task_ws).unwrap_or_default();
            if let Some(w) = m.pane.as_ref().and_then(|p| pane_ws.get(p)) {
                ws.insert(w.clone());
            }
            if let Some(w) = m
                .run
                .as_deref()
                .and_then(|r| runs_by_id.get(r))
                .and_then(|r| pane_ws.get(&r.pane))
            {
                ws.insert(w.clone());
            }
            (k.clone(), ws)
        })
        .collect();
    Collected {
        items,
        meta,
        notes,
        complete,
        stale_tasks,
        item_ws,
    }
}

pub async fn attention_api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "attention.list" => attention_list(server, ctx, p),
        "attention.update" => attention_update(server, ctx, p),
        "attention.batch" => attention_ext::batch_api(server, ctx, p),
        _ => return None,
    })
}

fn key_json(m: &Meta) -> Value {
    json!({"kind": m.kind, "id": m.id})
}

/// `attention.list {budget_ms?, effort?}` (15 §8). `effort` caps the coarse effort of
/// non-urgent shortlist entries; either parameter enables the five-minute view.
pub fn attention_list(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let now_ms = now();
    // Authorization before retrieval (15 §11): resolve the caller's scope first, then only
    // project items inside it; report how many were excluded, never their contents.
    let scope = caller_workspace(server, ctx)?;
    let mut col = collect(server, now_ms);
    for t in &col.stale_tasks {
        spawn_refresh(server, t);
    }
    let mut excluded = 0usize;
    if let Some(ws) = &scope {
        let item_ws = std::mem::take(&mut col.item_ws);
        let visible = |id: &str| item_ws.get(id).is_some_and(|s| s.contains(ws));
        let before = col.items.len();
        col.items.retain(|i| visible(&i.key.object_id));
        excluded = before - col.items.len();
        col.meta.retain(|id, _| visible(id));
        if excluded > 0 {
            col.notes.push(format!(
                "excluded {excluded} item(s) outside this pane's scope (workspace)"
            ));
        }
    }
    let aprefs = attention_ext::prefs();
    let ranked = att::rank(&col.items, now_ms, &aprefs);
    // Lane 2C: deadlines, batches and the Also working footer.
    let afacts = attention_ext::facts(server);
    let batches = attention_ext::batches(&ranked);
    // T4 (§8.2): the deterministic estimate for tasks whose effort the user hasn't set, shown
    // with its source; ranking and the five-minute view keep using the user's value.
    let heuristics: HashMap<String, String> = server.with_core(|c| {
        c.store
            .load::<Projection>(K_PROJ)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| p.effort_heuristic.map(|e| (p.task, e)))
            .collect()
    });
    let items: Vec<Value> = ranked
        .iter()
        .filter_map(|r| {
            let m = col.meta.get(&r.item.key.object_id)?;
            let extras = attention_ext::item_extras(&afacts, m, r, &batches, now_ms);
            let woke = m
                .woke
                .clone()
                .or_else(|| r.woke_from_snooze.then(|| "Became urgent".to_string()));
            Some(json!({
                "key": key_json(m),
                "class": if r.class == att::AttentionClass::FinishedTurns { json!("finished_turns") } else { json!(r.class.number()) },
                "title": m.title,
                "subtitle": m.subtitle,
                "task": m.task,
                "run": m.run,
                "pane": m.pane,
                "interaction": m.interaction,
                "explanation": r.explanation.replace("; ", " · "),
                "age_ms": (now_ms - r.item.opened_at_ms).max(0),
                "risk": r.item.risk.map(risk_str),
                "effort": (r.item.kind != AttentionKind::FinishedTurn).then(|| effort_str(r.item.effort)),
                "effort_estimate": (r.item.kind != AttentionKind::FinishedTurn && r.item.effort == Effort::Unknown)
                    .then(|| m.task.as_ref().and_then(|t| heuristics.get(t)))
                    .flatten()
                    .map(|e| json!({"effort": e, "source": "heuristic"})),
                "blocks_tasks": r.item.blocks_tasks,
                "snoozed_until_ms": m.snoozed_until_ms,
                "woke_from_snooze": woke,
                "urgent": r.class.is_urgent(),
                "deadline_ms": extras["deadline_ms"],
                "deadline_source": extras["deadline_source"],
                "deadline_in_ms": extras["deadline_in_ms"],
                "batch": extras["batch"],
            }))
        })
        .collect();
    let budget = p.get("budget_ms").and_then(Value::as_i64);
    let max_effort = s(p, "effort").map(|e| effort_of(Some(e)));
    let five = (budget.is_some() || max_effort.is_some()).then(|| {
        let budget = budget.unwrap_or(5 * 60 * 1000);
        let (eligible, dropped): (Vec<_>, Vec<_>) = ranked.iter().cloned().partition(|r| {
            r.class.is_urgent()
                || max_effort.is_none_or(|m| {
                    m == Effort::Unknown || r.item.effort.nominal_ms() <= m.nominal_ms()
                })
        });
        let dropped = dropped
            .iter()
            .filter(|r| r.class != att::AttentionClass::FinishedTurns)
            .count();
        let v = att::five_minute_view(&eligible, budget);
        let omitted = v.omitted_count + dropped;
        let mut note = v.note.clone();
        if dropped > 0 && v.omitted_count == 0 {
            note.push_str(&format!(" {dropped} more in All items."));
        }
        let urgent_notes: Vec<Value> = v
            .items
            .iter()
            .filter_map(|e| {
                let m = col.meta.get(&e.ranked.item.key.object_id)?;
                e.note.as_ref().map(|n| json!({"key": key_json(m), "note": n}))
            })
            .collect();
        json!({
            "keys": v.items.iter().filter_map(|e| col.meta.get(&e.ranked.item.key.object_id).map(key_json)).collect::<Vec<_>>(),
            "omitted_count": omitted,
            "note": note,
            "item_notes": urgent_notes,
        })
    });
    Ok(json!({
        "items": items,
        "coverage": {
            "complete": col.complete,
            "notes": col.notes,
            "scope": scope.as_ref().map_or_else(|| json!("all"), |w| json!({"workspace": w})),
            "excluded": excluded,
        },
        "five_minute": five,
        "batches": batches.iter().map(|(id, ms)| json!({"id": id, "members": ms.iter().filter_map(|o| col.meta.get(o).map(key_json)).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        "also_working": attention_ext::also_working(server, scope.as_deref(), now_ms),
    }))
}

const KEY_KINDS: &[&str] = &[
    "interaction",
    "review",
    "check_failed",
    "send_unknown",
    "finished_turn",
    "binding_suspended",
];

/// `attention.update {key, seen?, snooze_until_ms?, pin?, item_rev?}`: per-user preference only;
/// never answers, sends or accepts (15 §8.3).
fn attention_update(server: &Arc<Server>, _ctx: &Ctx, p: &Value) -> R {
    let key = p.get("key").ok_or_else(|| invalid("missing param `key`"))?;
    let kind = req(key, "kind")?;
    let id = req(key, "id")?;
    if !KEY_KINDS.contains(&kind) {
        return Err(invalid(format!("unknown attention item kind `{kind}`")));
    }
    let obj = format!("{kind}:{id}");
    let now_ms = now();
    let col = collect(server, now_ms);
    let item = col.items.iter().find(|i| i.key.object_id == obj).cloned();
    let mut pref = server
        .with_core(|c| c.store.get::<Pref>(K_PREF, &obj).ok().flatten())
        .unwrap_or(Pref {
            key: obj.clone(),
            ..Default::default()
        });
    let mut warning = None;
    let mut read_mark = None;
    match p.get("seen").and_then(Value::as_bool) {
        Some(true) => {
            let rev = u(p, "item_rev")
                .or(item.as_ref().map(|i| i.key.revision))
                .unwrap_or(0);
            pref.seen_rev = Some(rev);
            if kind == "finished_turn"
                && let Some(r) = server.with_core(|c| c.run(id).cloned())
            {
                read_mark = Some((r.pane.clone(), rev));
            }
        }
        Some(false) => pref.seen_rev = None,
        None => {}
    }
    match p.get("snooze_until_ms") {
        Some(Value::Null) => {
            pref.snoozed_until_ms = None;
            pref.snapshot = None;
        }
        Some(v) => {
            let until = v
                .as_i64()
                .ok_or_else(|| invalid("snooze_until_ms: epoch ms or null"))?;
            pref.snoozed_until_ms = Some(until);
            pref.snapshot = item.clone();
            warning = item
                .as_ref()
                .and_then(|i| att::snooze_warning(i, until, now_ms));
        }
        None => {}
    }
    if let Some(pin) = p.get("pin").and_then(Value::as_bool) {
        pref.pinned = pin;
    }
    pref.updated_at_ms = now_ms;
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.put(K_PREF, &obj, None, &pref);
    if let Some((pane, rev)) = read_mark {
        tx.m.read_mark("local", &pane, rev);
    }
    tx.event(
        "attention.preference_changed",
        json!({"key": {"kind": kind, "id": id}}),
        json!({"seen": pref.seen_rev.is_some(), "snoozed_until_ms": pref.snoozed_until_ms, "pinned": pref.pinned}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(json!({
        "key": {"kind": kind, "id": id},
        "seen": pref.seen_rev.is_some(),
        "seen_rev": pref.seen_rev,
        "snoozed_until_ms": pref.snoozed_until_ms,
        "pinned": pref.pinned,
        "warning": warning,
    }))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod alias_tests {
    use super::*;

    #[test]
    fn client_spellings_and_flat_aliases() {
        let p = normalize(
            "task.review.accept",
            &json!({"expected_intent_revision": 2, "expected_package_revision": 9}),
        );
        assert_eq!(p["intent_revision"], 2);
        assert_eq!(p["package_revision"], 9);
        let p = normalize("task.check.run", &json!({"subject_id": "s1"}));
        assert_eq!(p["subject"], "s1");
        let j = with_aliases(json!({
            "package_revision": 4,
            "accept_capable": true,
            "subject": {"id": "s1"},
            "intent": {"criteria": [{"id": "c1", "text": "Preserve SSO"}]},
            "assessment": {"criteria": [{"criterion_id": "c1", "status": "missing", "required": true, "evaluation": "check", "evidence_refs": []}], "blockers": []},
            "observed_commands": [{"command": "cargo test"}],
            "claims": [{"command": "tests pass"}],
        }));
        assert_eq!(j["revision"], 4);
        assert_eq!(j["criteria"][0]["text"], "Preserve SSO");
        assert_eq!(j["criteria"][0]["needs_exception"], true);
        assert_eq!(j["observed"][1]["category"], "claim");
        assert_eq!(j["observed"][1]["text"], "tests pass");
        assert_eq!(j["subject"]["accept_capable"], true);
    }
}
