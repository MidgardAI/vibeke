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
//! handlers use `spawn_blocking`, hooks spawn threads.

use crate::Server;
use crate::api::{Ctx, R, err, internal, invalid, not_found, req, s, u};
use crate::core::Tx;
use crate::tracking::{self, MessageState};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
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

/// Tasks with a refresh running → whether another refresh was requested meanwhile.
fn refreshing() -> &'static Mutex<HashMap<String, bool>> {
    static M: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

fn load_all<T: DeserializeOwned>(c: &crate::core::Core, kind: &str) -> Vec<T> {
    let mut v: Vec<T> = c.store.load(kind).unwrap_or_default();
    v.extend(c.store.load_closed(kind, 5000).unwrap_or_default());
    v
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
    let specs = check_specs_at(&root, &subject.head_sha);
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
            "defined_at": subject.head_sha,
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
}

fn checkout_of(task: &Task) -> Option<PathBuf> {
    let p = PathBuf::from(task.worktree_path.as_deref().unwrap_or(&task.repo_root));
    p.is_dir().then_some(p)
}

/// The proposed review base (15 §5): the base recorded at tracking time, the owned task's
/// resolved base, or the merge-base/HEAD fallback.
fn base_for(server: &Server, task: &Task, path: &Path) -> (Option<String>, Value) {
    let stored = tracking::baseline_of(server, &task.id);
    if let Some(b) = stored
        .as_ref()
        .and_then(|v| v.get("review_base"))
        .filter(|b| !b.is_null())
        && let Some(sha) = b.get("base_sha").and_then(Value::as_str)
        && subject::rev_parse(path, sha).is_ok()
    {
        return (Some(sha.to_string()), b.clone());
    }
    let owned = (task.ownership == TaskOwnership::Owned)
        .then_some(task.base_ref.as_deref())
        .flatten();
    match subject::propose_review_base(path, owned, None, None) {
        Ok(p) => (Some(p.base_sha.clone()), json!(p)),
        Err(e) => (None, json!({"error": e.to_string()})),
    }
}

fn candidates_blocking(server: &Server, task: &Task, bs: &[TaskRunBinding]) -> Candidates {
    let checkout = checkout_of(task);
    let live = bs.iter().any(|b| b.state != BindingState::Closed)
        || (bs.is_empty() && task.ownership == TaskOwnership::Owned);
    let mut ends: Vec<(EndRec, Option<ChangeSubject>)> = server.with_core(|c| {
        let mut v: Vec<EndRec> = load_all(c, K_END);
        v.retain(|e| e.task == task.id);
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
    };
    let Some(path) = checkout else {
        out.warnings
            .push("Checkout unavailable; only retained candidates can be inspected".into());
        out.current = out.ends.iter().rev().find_map(|(_, s)| s.clone());
        return out;
    };
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
                        ))
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
    out
}

/// Pin the end-boundary candidate of a closed binding (15 §4.2), or record that there is none.
pub fn pin_end(server: &Server, b: &TaskRunBinding) {
    if server.with_core(|c| c.store.get::<Value>(K_END, &b.id).ok().flatten().is_some()) {
        return;
    }
    let Ok(task) = tracking::find_task(server, &b.task_id) else {
        return;
    };
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
    match checkout_of(&task) {
        None => rec.note = Some("No bound end candidate: checkout unavailable".into()),
        Some(path) => {
            let (base, _) = base_for(server, &task, &path);
            let head = subject::rev_parse(&path, "HEAD").ok();
            let dirty = subject::observation_baseline(&path)
                .map(|b| b.dirty_state)
                .unwrap_or(DirtyState::Unknown);
            match (base, head) {
                (Some(base), Some(head)) if base != head => {
                    match subject::capture_committed(&path, &base, &head) {
                        Ok(s) => {
                            rec.subject_id = Some(s.id.clone());
                            rec.head_sha = Some(s.head_sha.clone());
                            if dirty != DirtyState::Clean {
                                rec.note = Some(
                                    "Uncommitted changes at close are not part of this candidate"
                                        .into(),
                                );
                            }
                            subj = Some(s);
                        }
                        Err(e) => rec.note = Some(format!("No bound end candidate: {e}")),
                    }
                }
                _ => {
                    rec.note = Some(if dirty == DirtyState::Clean {
                        "No bound end candidate: no commits since the review base".into()
                    } else {
                        "No bound end candidate: only uncommitted work; choose a committed range later"
                            .into()
                    })
                }
            }
        }
    }
    let mut c = server.core.lock().unwrap();
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
    tx.m.close(K_END, &b.id, None, &rec);
    tx.event(
        "review.end_candidate_pinned",
        json!({"task": task.id, "binding": b.id, "run": b.run_id}),
        json!({"subject": rec.subject_id, "note": rec.note}),
    );
    let _ = server.commit(&mut c, tx);
}

/// Hook: bindings closed (unbind / switch). Pins their end candidates off the state path.
pub fn on_bindings_closed(server: &Arc<Server>, closed: Vec<TaskRunBinding>) {
    if closed.is_empty() {
        return;
    }
    let srv = server.clone();
    std::thread::spawn(move || {
        let mut tasks = BTreeSet::new();
        for b in &closed {
            pin_end(&srv, b);
            tasks.insert(b.task_id.clone());
        }
        for t in tasks {
            spawn_refresh(&srv, &t);
        }
    });
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
    if tasks.is_empty() {
        return;
    }
    // The adapter applies the idle state right after reporting Stop; let it settle first.
    let srv = server.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        for t in tasks {
            spawn_refresh(&srv, &t);
        }
    });
}

/// Recompute a task's package off the state path and persist label/projection/invalidation.
/// Coalesces concurrent requests for the same task.
pub fn spawn_refresh(server: &Arc<Server>, task: &str) {
    {
        let mut m = refreshing().lock().unwrap();
        if let Some(again) = m.get_mut(task) {
            *again = true;
            return;
        }
        m.insert(task.to_string(), false);
    }
    let srv = server.clone();
    let task = task.to_string();
    std::thread::spawn(move || {
        loop {
            if let Ok(pkg) = build_package(&srv, &task, None) {
                persist(&srv, &pkg);
            }
            let mut m = refreshing().lock().unwrap();
            if m.get(&task) == Some(&true) {
                m.insert(task.clone(), false);
                continue;
            }
            m.remove(&task);
            break;
        }
    });
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

/// Live facts for readiness (15 §7): bound runs, other active writers in the same checkout,
/// open interactions on bound runs, pending switches and unresolved deliveries.
fn live_state(
    server: &Server,
    task: &Task,
    bs: &[TaskRunBinding],
    checkout: Option<&Path>,
) -> LiveState {
    let active: Vec<&TaskRunBinding> = bs
        .iter()
        .filter(|b| b.state == BindingState::Active && b.role == BindingRole::Implementation)
        .collect();
    let (runs, interactions, pending, panes) = server.with_core(|c| {
        (
            c.model.runs.clone(),
            c.model.interactions.clone(),
            active
                .iter()
                .filter(|b| {
                    c.store
                        .kv_get("tracking", &tracking::pending_key(&b.run_id))
                        .ok()
                        .flatten()
                        .is_some()
                })
                .count(),
            c.model
                .panes
                .iter()
                .map(|p| (p.id.clone(), p.cwd.clone()))
                .collect::<HashMap<_, _>>(),
        )
    });
    let bound: BTreeSet<&str> = active.iter().map(|b| b.run_id.as_str()).collect();
    let bound_runs: Vec<BoundRun> = active
        .iter()
        .map(|b| BoundRun {
            run_id: b.run_id.clone(),
            activity: runs
                .iter()
                .find(|r| r.id == b.run_id)
                .map(|r| activity(&r.execution.value))
                .unwrap_or(RunActivity::Exited),
        })
        .collect();
    let checkout = checkout.map(|p| canon(&p.to_string_lossy()));
    let known_writers = runs
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
                .or_else(|| panes.get(&r.pane).cloned().flatten());
            match (&checkout, cwd) {
                (Some(co), Some(cwd)) => canon(&cwd).starts_with(co),
                _ => false,
            }
        })
        .map(|r| r.handle.clone())
        .collect();
    let open_interactions = interactions
        .iter()
        .filter(|i| i.status == InteractionStatus::Open && bound.contains(i.run.as_str()))
        .map(|i| i.id.clone())
        .collect();
    let unresolved_deliveries = tracking::messages_of(server, &task.id)
        .into_iter()
        .filter(|m| {
            matches!(
                m.state,
                MessageState::Sending | MessageState::DeliveryUnknown
            )
        })
        .map(|m| m.id)
        .collect();
    LiveState {
        turn_finished: runs
            .iter()
            .any(|r| bound.contains(r.id.as_str()) && r.turns_completed > 0),
        has_inspectable_changes: false,
        subject_is_current: false,
        sources_verified: true,
        bound_runs,
        known_writers,
        open_interactions,
        pending_binding_switch: pending > 0,
        unresolved_deliveries,
        open_blocking_concerns: vec![],
        check_definition_digests: BTreeMap::new(),
        current_environment_digests: BTreeMap::new(),
        environment_unavailable: BTreeSet::new(),
        external_outcome_changed: false,
        acceptance: None,
    }
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
        let mut v: Vec<CheckRunRec> = load_all(c, K_CHECK);
        v.retain(|r| r.task == task);
        v.sort_by(|a, b| a.run.id.cmp(&b.run.id));
        v.dedup_by(|a, b| a.run.id == b.run.id);
        v
    })
}

fn task_grants(server: &Server, task: &str) -> Vec<GrantRec> {
    server.with_core(|c| {
        let mut v: Vec<GrantRec> = load_all(c, K_GRANT);
        v.retain(|g| g.task == task);
        v
    })
}

fn latest_acceptance(server: &Server, task: &str) -> Option<AcceptanceRec> {
    server.with_core(|c| {
        let v: Vec<AcceptanceRec> = load_all(c, K_ACCEPT);
        v.into_iter()
            .filter(|a| a.acceptance.task_id == task)
            .max_by_key(|a| a.acceptance.accepted_at_ms)
    })
}

fn acceptance_history(server: &Server, task: &str) -> Vec<AcceptanceRec> {
    server.with_core(|c| {
        let mut v: Vec<AcceptanceRec> = load_all(c, K_ACCEPT);
        v.retain(|a| a.acceptance.task_id == task);
        v.sort_by_key(|a| a.acceptance.accepted_at_ms);
        v.dedup_by(|a, b| a.acceptance.id == b.acceptance.id);
        v
    })
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
        "accept_capable": s.is_committed() && current,
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
    let intent = task
        .intent_revision
        .and_then(|r| tracking::intent_at(server, &task.id, r));
    let bs = tracking::task_bindings(server, &task.id);
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
    let (observed, claims) = observed_table(server, &bs);
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

    let mut live = live_state(server, &task, &bs, cands.checkout.as_deref());
    live.subject_is_current = subject_is_current;
    live.has_inspectable_changes =
        cands.current.is_some() || cands.live_subject.is_some() || !observed.is_empty();
    live.check_definition_digests = digests.clone();
    let acc = latest_acceptance(server, &task.id);
    live.acceptance = acc.as_ref().map(|a| a.acceptance.clone());

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

    let diff_stat = selected
        .as_ref()
        .filter(|s| s.is_committed())
        .and_then(|s| subject::diff_stat(s).ok());

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
    } else if selected.as_ref().is_some_and(|s| !s.is_committed()) {
        Some("Select a committed revision to record acceptance")
    } else if !subject_is_current {
        Some("Only the current candidate can be accepted; earlier candidates are inspect only")
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

    let json = json!({
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
        "warnings": cands.warnings,
        "diff_stat": diff_stat,
        "observed_commands": observed.iter().map(command_json).collect::<Vec<_>>(),
        "claims": claims.iter().map(command_json).collect::<Vec<_>>(),
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
        "actions": {
            "accept": {
                "available": accept_reason.is_none(),
                "reason": accept_reason,
                "requires_exceptions": requires_exceptions,
            }
        },
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
        }
    });

    Ok(Pkg {
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
        if changed || old.as_ref().is_some_and(|o| o.explanation != p.explanation) {
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

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    let p = &normalize(method, p);
    Some(match method {
        "task.review.candidates" => review_candidates(server, p).await,
        "task.review.get" => review_get(server, p).await.map(with_aliases),
        "task.review.accept" => review_accept(server, ctx, p).await,
        "task.check.list" => check_list(server, p).await,
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
                Ok(_) => check_run(server, p).await,
                Err(e) => Err(e),
            }
        }
        "task.check.run" => check_run(server, p).await,
        "task.check.cancel" => check_cancel(server, p),
        "task.check.get" => check_get(server, p),
        _ => return None,
    })
}

async fn package(server: &Arc<Server>, task: &str, subject: Option<&str>) -> Result<Pkg, RpcError> {
    let srv = server.clone();
    let (task, subject) = (task.to_string(), subject.map(str::to_string));
    blocking(move || {
        let pkg = build_package(&srv, &task, subject.as_deref())?;
        persist(&srv, &pkg);
        Ok(pkg)
    })
    .await?
}

/// `task.review.candidates {task}`.
async fn review_candidates(server: &Arc<Server>, p: &Value) -> R {
    let pkg = package(server, req(p, "task")?, None).await?;
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
async fn review_get(server: &Arc<Server>, p: &Value) -> R {
    let pkg = package(server, req(p, "task")?, s(p, "subject")).await?;
    Ok(pkg.json)
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

/// `task.review.accept {task, intent_revision, subject_id, exceptions, package_revision?,
/// idempotency_key}` (15 §7).
async fn review_accept(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    if let Some(r) = tracking::replay(server, "task.review.accept", p) {
        return r;
    }
    let task_id = req(p, "task")?;
    let subject_id = req(p, "subject_id")?.to_string();
    let rev =
        u(p, "intent_revision").ok_or_else(|| invalid("missing param `intent_revision`"))? as u32;
    let exceptions = parse_exceptions(p)?;
    let pkg = package(server, task_id, None).await?;
    let stored_live = server.with_core(|c| {
        c.store
            .get::<ChangeSubject>(K_SUBJECT, &subject_id)
            .ok()
            .flatten()
            .is_some_and(|s| !s.is_committed())
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
    let snap = PackageSnapshot {
        task_id: &pkg.task.id,
        intent: &intent,
        package_revision: pkg.package_revision,
        subject: pkg.current.as_ref(),
        assessment: &pkg.assessment,
        sources_verified: true,
    };
    let acc = readiness::accept(&areq, &snap, now()).map_err(|e| {
        let mut d = serde_json::to_value(&e).unwrap_or(json!({}));
        d["reason"] = json!(e.reason());
        d["current_subject"] = json!(pkg.current.as_ref().map(|s| s.id.clone()));
        d["current_intent_revision"] = json!(intent.revision);
        d["package_revision"] = json!(pkg.package_revision);
        err(ErrorKind::Conflict, e.to_string()).details(d)
    })?;
    let mut c = server.core.lock().unwrap();
    // Serialize the expected-version check with the write (15 §7).
    let cur_task = c
        .task(&pkg.task.id)
        .cloned()
        .ok_or_else(|| not_found("task", &pkg.task.id))?;
    if cur_task.intent_revision != Some(intent.revision) {
        return Err(conflict(
            "review_changed",
            format!(
                "intent changed to revision {:?} during acceptance",
                cur_task.intent_revision
            ),
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
    tracking::record(&mut tx, "task.review.accept", p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    spawn_refresh(server, &t.id);
    Ok(result)
}

/// `task.check.list {task, subject?}`.
async fn check_list(server: &Arc<Server>, p: &Value) -> R {
    let pkg = package(server, req(p, "task")?, s(p, "subject")).await?;
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
    if let Some(r) = tracking::replay(server, "task.check.authorize", p) {
        return r;
    }
    let subject_id = req(p, "subject")?;
    let pkg = package(server, req(p, "task")?, Some(subject_id)).await?;
    let subj = pkg
        .selected
        .clone()
        .ok_or_else(|| not_found("subject", subject_id))?;
    if !subj.is_committed() {
        return Err(conflict(
            "verification_unbound",
            "Select a committed revision to verify",
        ));
    }
    let entry = find_entry(&pkg, req(p, "check")?)?;
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
    let mut tx = Tx::new();
    tx.m.close(K_GRANT, &grant.id, None, &rec);
    tx.event_by(
        "check.authorized",
        json!({"task": pkg.task.id, "check": entry.def.id}),
        json!({"kind": "user", "id": grant.authorized_by.id}),
        json!({"subject": subj.id, "definition_digest": entry.def.definition_digest, "trust": entry.def.trust}),
    );
    tracking::record(&mut tx, "task.check.authorize", p, &result);
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
async fn check_run(server: &Arc<Server>, p: &Value) -> R {
    if let Some(r) = tracking::replay(server, "task.check.run", p) {
        return r;
    }
    let subject_id = req(p, "subject")?;
    let pkg = package(server, req(p, "task")?, Some(subject_id)).await?;
    let subj = pkg
        .selected
        .clone()
        .ok_or_else(|| not_found("subject", subject_id))?;
    let entry = find_entry(&pkg, req(p, "check")?)?;
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
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        put_check(&mut tx, &rec);
        tx.event(
            "check.queued",
            json!({"task": rec.task, "check": rec.run.check_id, "check_run": rec.run.id}),
            json!({"subject": subj.id, "head": subj.head_sha}),
        );
        tracking::record(&mut tx, "task.check.run", p, &result);
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
    let opts = checks::RunOptions {
        idempotency_key: rec.run.idempotency_key.clone(),
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
fn check_cancel(server: &Arc<Server>, p: &Value) -> R {
    let id = req(p, "check_run")?;
    let rec = load_check(server, id)?;
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
fn check_get(server: &Server, p: &Value) -> R {
    let rec = load_check(server, req(p, "check_run")?)?;
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
pub fn recover(server: &Server) {
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
}

/// Gather attention items from the model and cached projections (no git, one lock).
fn collect(server: &Server, now_ms: i64) -> Collected {
    let machine = server.opts.machine.clone();
    let inflight_cutoff = now_ms - 24 * 3600 * 1000;
    let (ints, runs, tasks, reads, open_bindings, projections, messages, prefs, closed_ints) =
        server.with_core(|c| {
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
            effort: Effort::Unknown,
            seen: false,
            snoozed_until_ms: None,
            pinned: false,
            unsafe_source: false,
            interaction: None,
        }
    };

    // Open interactions.
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
            let mut sub = vec![p.label_text.clone()];
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

    // Per-user preferences: seen, pin, snooze with material wake-ups.
    let aprefs = att::AttentionPrefs::default();
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
                    m.woke = Some(w.text(now_ms));
                    it.snoozed_until_ms = None;
                }
            }
        }
    }

    let complete = stale_tasks.is_empty();
    if !complete {
        notes.push(format!(
            "{} tracked task(s) have no review projection yet; refreshing",
            stale_tasks.len()
        ));
    }
    Collected {
        items,
        meta,
        notes,
        complete,
        stale_tasks,
    }
}

pub async fn attention_api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "attention.list" => attention_list(server, p),
        "attention.update" => attention_update(server, ctx, p),
        _ => return None,
    })
}

fn key_json(m: &Meta) -> Value {
    json!({"kind": m.kind, "id": m.id})
}

/// `attention.list {budget_ms?, effort?}` (15 §8). `effort` caps the coarse effort of
/// non-urgent shortlist entries; either parameter enables the five-minute view.
pub fn attention_list(server: &Arc<Server>, p: &Value) -> R {
    let now_ms = now();
    let col = collect(server, now_ms);
    for t in &col.stale_tasks {
        spawn_refresh(server, t);
    }
    let aprefs = att::AttentionPrefs::default();
    let ranked = att::rank(&col.items, now_ms, &aprefs);
    let items: Vec<Value> = ranked
        .iter()
        .filter_map(|r| {
            let m = col.meta.get(&r.item.key.object_id)?;
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
                "snoozed_until_ms": m.snoozed_until_ms,
                "woke_from_snooze": woke,
                "urgent": r.class.is_urgent(),
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
        "coverage": {"complete": col.complete, "notes": col.notes},
        "five_minute": five,
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
            "claims": [{"text": "tests pass"}],
        }));
        assert_eq!(j["revision"], 4);
        assert_eq!(j["criteria"][0]["text"], "Preserve SSO");
        assert_eq!(j["criteria"][0]["needs_exception"], true);
        assert_eq!(j["observed"][1]["category"], "claim");
        assert_eq!(j["subject"]["accept_capable"], true);
    }
}
