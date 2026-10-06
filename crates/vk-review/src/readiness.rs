//! Criterion assessment, readiness labels, acceptance and freshness (§6.1, §6.2, §7).
//!
//! Rules implemented here:
//! - Evidence is aggregated by **check definition + environment + subject**.
//! - Only evidence bound to the current subject by its runner/collector (a Vibeke verification,
//!   or an observed command with an established subject) can support a machine criterion.
//! - Agent claims never support anything. Unbound observed commands read
//!   **Command passed · code binding unverified** and leave the criterion `unknown`.
//! - Mixed pass/fail on the same key → `needs_judgment`; a retry never erases a failure.
//! - Failures on older subjects stay visible as history and do not fail a fixed revision.
//! - Ready requires every live condition in [`LiveState`] to be clear; idleness alone is never
//!   enough.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::checks::{CheckRun, CheckState, CommandCategory, CommandOutcome, ObservedCommand};
use crate::intent::{Criterion, Evaluation, StopAt, TaskIntent};
use crate::subject::ChangeSubject;
use crate::{Actor, ActorKind, new_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceCategory {
    /// Prose or unconfirmed report; never support.
    AgentClaim,
    /// Observed agent check (bound only when `subject_id` was established by the collector).
    Observed,
    /// Vibeke verification in a disposable checkout of the subject.
    VibekeVerification,
    /// A human's recorded judgment of a criterion on a subject.
    HumanReview,
    /// External outcome observation (PR state, …) for a subject.
    ExternalObservation,
    /// Pre-spec-15 evidence lacking binding fields: **Legacy evidence — binding incomplete**.
    Legacy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceOutcome {
    Passed,
    Failed,
    /// Cancelled, interrupted, unknown, or a failed/offline lookup.
    Unknown,
}

/// One piece of evidence as the readiness engine sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub id: String,
    pub category: EvidenceCategory,
    pub outcome: EvidenceOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_definition_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_digest: Option<String>,
    /// Subject established for this evidence; `None` = unbound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    /// Explicit criterion links (human reviews, external observations, claims). Check evidence
    /// links through the criterion's `check_definition_ids` instead.
    #[serde(default)]
    pub criterion_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<Actor>,
    pub observed_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl Evidence {
    /// Evidence from a terminal Vibeke check run.
    pub fn from_check_run(run: &CheckRun) -> Evidence {
        Evidence {
            id: run.id.clone(),
            category: EvidenceCategory::VibekeVerification,
            outcome: match run.state {
                CheckState::Passed => EvidenceOutcome::Passed,
                CheckState::Failed => EvidenceOutcome::Failed,
                _ => EvidenceOutcome::Unknown,
            },
            check_definition_id: Some(run.check_id.clone()),
            definition_digest: Some(run.definition_digest.clone()),
            environment_digest: run.environment.as_ref().map(|e| e.digest.clone()),
            subject_id: Some(run.subject_id.clone()),
            criterion_ids: vec![],
            actor: Some(run.authorized_by.clone()),
            observed_at_ms: run.ended_at_ms.or(run.started_at_ms).unwrap_or(0),
            summary: None,
        }
    }

    /// Evidence from an observed-command row. `check` links the row to a defined check
    /// (definition id + digest) when the user/recipe mapping matched it.
    pub fn from_observed(cmd: &ObservedCommand, check: Option<(&str, &str)>) -> Evidence {
        Evidence {
            id: cmd.id.clone(),
            category: match cmd.category {
                CommandCategory::AgentClaim => EvidenceCategory::AgentClaim,
                CommandCategory::Observed => EvidenceCategory::Observed,
                CommandCategory::VibekeVerification => EvidenceCategory::VibekeVerification,
            },
            outcome: match cmd.outcome {
                CommandOutcome::Passed => EvidenceOutcome::Passed,
                CommandOutcome::Failed => EvidenceOutcome::Failed,
                CommandOutcome::Unknown => EvidenceOutcome::Unknown,
            },
            check_definition_id: check.map(|c| c.0.to_string()),
            definition_digest: check.map(|c| c.1.to_string()),
            environment_digest: None,
            subject_id: cmd.subject_id.clone(),
            criterion_ids: vec![],
            actor: Some(Actor::agent(cmd.run_id.clone())),
            observed_at_ms: cmd.ended_at_ms.or(cmd.started_at_ms).unwrap_or(0),
            summary: Some(cmd.command.clone()),
        }
    }

    fn applies_to(&self, c: &Criterion) -> bool {
        self.criterion_ids.iter().any(|id| id == &c.id)
            || self
                .check_definition_id
                .as_ref()
                .is_some_and(|d| c.check_definition_ids.contains(d))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunActivity {
    Idle,
    Working,
    Disconnected,
    Exited,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundRun {
    pub run_id: String,
    pub activity: RunActivity,
}

/// Live facts the server fills in for an assessment.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LiveState {
    /// A bound run's turn completed (for the `turn_finished` label).
    #[serde(default)]
    pub turn_finished: bool,
    /// There is an inspectable diff/observation set (untracked or unconfirmed work).
    #[serde(default)]
    pub has_inspectable_changes: bool,
    /// The assessed subject is the current end-boundary candidate of the bound source (the
    /// checkout has not moved past it).
    #[serde(default)]
    pub subject_is_current: bool,
    /// Every source could be revalidated (machine online, no truncated cursor).
    #[serde(default)]
    pub sources_verified: bool,
    /// Bound implementation runs and their activity. Ready requires all `idle`/`exited`.
    #[serde(default)]
    pub bound_runs: Vec<BoundRun>,
    /// Other known active writers in the same checkout (other tasks, untracked runs, writing
    /// shell processes).
    #[serde(default)]
    pub known_writers: Vec<String>,
    /// Open task Interactions.
    #[serde(default)]
    pub open_interactions: Vec<String>,
    #[serde(default)]
    pub pending_binding_switch: bool,
    /// Task messages in `sending` or `delivery_unknown`.
    #[serde(default)]
    pub unresolved_deliveries: Vec<String>,
    /// User-marked blockers and unassessed potentially blocking reviewer concerns.
    #[serde(default)]
    pub open_blocking_concerns: Vec<String>,
    /// Current digest of each defined check (check id → digest).
    #[serde(default)]
    pub check_definition_digests: BTreeMap<String, String>,
    /// Current environment digest per check. Evidence from another environment is stale; a
    /// check without an entry has unknown environment identity, so its evidence has unknown
    /// freshness (never "unconstrained").
    #[serde(default)]
    pub current_environment_digests: BTreeMap<String, String>,
    /// Checks whose current environment identity is explicitly unavailable: their evidence
    /// and any acceptance relying on them have unknown freshness.
    #[serde(default)]
    pub environment_unavailable: BTreeSet<String>,
    /// A relevant external outcome (e.g. PR head) changed since acceptance.
    #[serde(default)]
    pub external_outcome_changed: bool,
    /// Latest acceptance recorded for this task, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<ReviewAcceptance>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessLabel {
    TurnFinished,
    NeedsTaskDetails,
    ChangesToInspect,
    ReviewAvailable,
    ReadyForReview,
    Reviewed,
    ReviewOutdated,
}

impl ReadinessLabel {
    pub fn text(self) -> &'static str {
        match self {
            ReadinessLabel::TurnFinished => "Turn finished",
            ReadinessLabel::NeedsTaskDetails => "Needs task details",
            ReadinessLabel::ChangesToInspect => "Changes to inspect",
            ReadinessLabel::ReviewAvailable => "Review available",
            ReadinessLabel::ReadyForReview => "Ready for your review",
            ReadinessLabel::Reviewed => "Reviewed",
            ReadinessLabel::ReviewOutdated => "Review outdated",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionStatus {
    Supported,
    Missing,
    Stale,
    Unknown,
    NeedsJudgment,
    Failed,
}

impl CriterionStatus {
    /// Combination severity (higher dominates).
    fn severity(self) -> u8 {
        match self {
            CriterionStatus::Supported => 0,
            CriterionStatus::Missing => 1,
            CriterionStatus::Stale => 2,
            CriterionStatus::Unknown => 3,
            CriterionStatus::NeedsJudgment => 4,
            CriterionStatus::Failed => 5,
        }
    }
    pub fn text(self) -> &'static str {
        match self {
            CriterionStatus::Supported => "PASS",
            CriterionStatus::Missing => "MISSING",
            CriterionStatus::Stale => "STALE",
            CriterionStatus::Unknown => "UNKNOWN",
            CriterionStatus::NeedsJudgment => "NEEDS JUDGMENT",
            CriterionStatus::Failed => "FAILED",
        }
    }
}

/// Aggregation key of supporting check evidence.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CheckKey {
    pub check_definition_id: String,
    pub definition_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionAssessment {
    pub criterion_id: String,
    pub version: u32,
    pub required: bool,
    pub evaluation: Evaluation,
    pub status: CriterionStatus,
    pub reasons: Vec<String>,
    pub evidence_refs: Vec<String>,
    /// Check keys whose evidence supports this criterion on the current subject.
    #[serde(default)]
    pub supporting_checks: Vec<CheckKey>,
}

impl CriterionAssessment {
    /// Whether a required criterion in this state needs an explicit exception to be accepted
    /// (and blocks Ready). Human/external `needs_judgment` is the review itself.
    pub fn needs_exception(&self) -> bool {
        match self.status {
            CriterionStatus::Supported => false,
            CriterionStatus::NeedsJudgment => self.evaluation == Evaluation::Check,
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerKind {
    IntentMissing,
    MappingsUnconfirmed,
    StopUnspecified,
    NoSubject,
    SubjectNotCommitted,
    SubjectNotCurrent,
    SourcesUnverified,
    RequiredCriterion,
    TrustedCheckFailed,
    BlockingConcern,
    RunActive,
    KnownWriter,
    OpenInteraction,
    PendingBindingSwitch,
    UnresolvedDelivery,
}

/// Something that prevents **Ready for your review**.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocker {
    pub kind: BlockerKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_id: Option<String>,
    pub message: String,
}

impl Blocker {
    fn new(kind: BlockerKind, ref_id: Option<&str>, message: impl Into<String>) -> Self {
        Blocker {
            kind,
            ref_id: ref_id.map(str::to_string),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAssessment {
    pub label: ReadinessLabel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_revision: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    pub criteria: Vec<CriterionAssessment>,
    /// Everything that currently prevents Ready (empty only when Ready, or Reviewed with a
    /// clean state).
    pub blockers: Vec<Blocker>,
    pub explanation: Vec<String>,
}

impl ReviewAssessment {
    pub fn criterion(&self, id: &str) -> Option<&CriterionAssessment> {
        self.criteria.iter().find(|c| c.criterion_id == id)
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

struct CheckResult {
    status: CriterionStatus,
    reasons: Vec<String>,
    refs: Vec<String>,
    supporting: Vec<CheckKey>,
}

fn assess_check_definition(
    def_id: &str,
    applicable: &[&Evidence],
    subject: &ChangeSubject,
    live: &LiveState,
) -> CheckResult {
    let mut reasons = Vec::new();
    let mut refs = Vec::new();
    let mut supporting = Vec::new();
    let ev: Vec<&Evidence> = applicable
        .iter()
        .copied()
        .filter(|e| {
            e.check_definition_id.as_deref() == Some(def_id)
                && matches!(
                    e.category,
                    EvidenceCategory::Observed | EvidenceCategory::VibekeVerification
                )
        })
        .collect();
    let current_digest = live.check_definition_digests.get(def_id);
    let current_env = live.current_environment_digests.get(def_id);
    // §7: unavailable environment identity yields unknown freshness. A missing current
    // identity is unknown, never "unconstrained".
    let env_unavailable = live.environment_unavailable.contains(def_id) || current_env.is_none();

    let on_subject: Vec<&Evidence> = ev
        .iter()
        .copied()
        .filter(|e| e.subject_id.as_deref() == Some(subject.id.as_str()))
        .collect();
    let (fresh, stale_here): (Vec<&Evidence>, Vec<&Evidence>) =
        on_subject.iter().copied().partition(|e| {
            current_digest.is_some()
                && !env_unavailable
                && e.definition_digest.as_ref() == current_digest
                && current_env.is_some_and(|env| e.environment_digest.as_ref() == Some(env))
        });

    // History: failures on other subjects stay visible but don't fail this revision.
    for e in ev.iter().filter(|e| {
        e.subject_id.is_some()
            && e.subject_id.as_deref() != Some(subject.id.as_str())
            && e.outcome == EvidenceOutcome::Failed
    }) {
        reasons.push(format!("{def_id}: failed on an older revision (history)"));
        refs.push(e.id.clone());
    }

    // Aggregate fresh evidence by environment (definition and subject are fixed here).
    let mut groups: BTreeMap<Option<&str>, (bool, bool, bool)> = BTreeMap::new();
    for e in &fresh {
        let g = groups
            .entry(e.environment_digest.as_deref())
            .or_insert((false, false, false));
        match e.outcome {
            EvidenceOutcome::Passed => g.0 = true,
            EvidenceOutcome::Failed => g.1 = true,
            EvidenceOutcome::Unknown => g.2 = true,
        }
        refs.push(e.id.clone());
    }
    let verb = |e: &&&Evidence| match e.category {
        EvidenceCategory::VibekeVerification => "Vibeke verification",
        _ => "Observed check",
    };

    let status = if !groups.is_empty() {
        let mixed = groups.values().any(|g| g.0 && g.1);
        let failed = groups.values().any(|g| g.1 && !g.0);
        let any_pass = groups.values().any(|g| g.0);
        if failed {
            reasons.push(format!(
                "{def_id}: failed on revision {}",
                short(&subject.head_sha)
            ));
            CriterionStatus::Failed
        } else if mixed {
            reasons.push(format!(
                "{def_id}: passed and failed on the same revision {} (possible flake); a retry does not erase the failure",
                short(&subject.head_sha)
            ));
            CriterionStatus::NeedsJudgment
        } else if any_pass && groups.values().all(|g| g.0) {
            let who = fresh
                .iter()
                .find(|e| e.outcome == EvidenceOutcome::Passed)
                .map(|e| verb(&e))
                .unwrap_or("Check");
            reasons.push(format!(
                "{def_id}: {who} passed on revision {}",
                short(&subject.head_sha)
            ));
            for (env, g) in &groups {
                if g.0 {
                    supporting.push(CheckKey {
                        check_definition_id: def_id.to_string(),
                        definition_digest: current_digest.cloned().unwrap_or_default(),
                        environment_digest: env.map(str::to_string),
                    });
                }
            }
            CriterionStatus::Supported
        } else {
            reasons.push(format!(
                "{def_id}: result unknown (cancelled, interrupted or unknown)"
            ));
            CriterionStatus::Unknown
        }
    } else {
        let unbound_pass: Vec<&&Evidence> = ev
            .iter()
            .filter(|e| e.subject_id.is_none() && e.outcome == EvidenceOutcome::Passed)
            .collect();
        let older_pass: Vec<&&Evidence> = ev
            .iter()
            .filter(|e| {
                e.subject_id.is_some()
                    && e.subject_id.as_deref() != Some(subject.id.as_str())
                    && e.outcome == EvidenceOutcome::Passed
            })
            .collect();
        if !unbound_pass.is_empty() {
            reasons.push(format!(
                "{def_id}: Command passed · code binding unverified"
            ));
            refs.extend(unbound_pass.iter().map(|e| e.id.clone()));
            CriterionStatus::Unknown
        } else if !stale_here.is_empty() && env_unavailable {
            reasons.push(format!(
                "{def_id}: environment identity unavailable; freshness unknown"
            ));
            refs.extend(stale_here.iter().map(|e| e.id.clone()));
            CriterionStatus::Unknown
        } else if !stale_here.is_empty() && current_digest.is_none() {
            reasons.push(format!("{def_id}: check definition identity unavailable"));
            refs.extend(stale_here.iter().map(|e| e.id.clone()));
            CriterionStatus::Unknown
        } else if !stale_here.is_empty() {
            reasons.push(format!(
                "{def_id}: check definition or environment changed since it ran"
            ));
            refs.extend(stale_here.iter().map(|e| e.id.clone()));
            CriterionStatus::Stale
        } else if !older_pass.is_empty() {
            reasons.push(format!("{def_id}: passed on an older revision only"));
            refs.extend(older_pass.iter().map(|e| e.id.clone()));
            CriterionStatus::Stale
        } else {
            reasons.push(format!(
                "{def_id}: no observed run on revision {}",
                short(&subject.head_sha)
            ));
            CriterionStatus::Missing
        }
    };
    CheckResult {
        status,
        reasons,
        refs,
        supporting,
    }
}

fn assess_criterion(
    c: &Criterion,
    subject: &ChangeSubject,
    evidence: &[Evidence],
    live: &LiveState,
) -> CriterionAssessment {
    let applicable: Vec<&Evidence> = evidence.iter().filter(|e| e.applies_to(c)).collect();
    let mut reasons = Vec::new();
    let mut refs = Vec::new();
    let mut supporting = Vec::new();

    for e in &applicable {
        match e.category {
            EvidenceCategory::AgentClaim => {
                reasons.push(format!(
                    "Agent claims{}; not evidence",
                    e.summary
                        .as_deref()
                        .map(|s| format!(" \"{s}\""))
                        .unwrap_or_default()
                ));
                refs.push(e.id.clone());
            }
            EvidenceCategory::Legacy => {
                reasons.push("Legacy evidence — binding incomplete".into());
                refs.push(e.id.clone());
            }
            _ => {}
        }
    }

    let latest_on_subject = |cat: EvidenceCategory| {
        applicable
            .iter()
            .filter(|e| e.category == cat && e.subject_id.as_deref() == Some(subject.id.as_str()))
            .max_by_key(|e| e.observed_at_ms)
            .copied()
    };
    let on_other_subject = |cat: EvidenceCategory| {
        applicable
            .iter()
            .any(|e| e.category == cat && e.subject_id.as_deref() != Some(subject.id.as_str()))
    };

    let status = match c.evaluation {
        Evaluation::Check => {
            if c.check_definition_ids.is_empty() {
                reasons.push("No check mapped to this criterion".into());
                CriterionStatus::Missing
            } else {
                let mut worst = CriterionStatus::Supported;
                for d in &c.check_definition_ids {
                    let r = assess_check_definition(d, &applicable, subject, live);
                    if r.status.severity() > worst.severity() {
                        worst = r.status;
                    }
                    reasons.extend(r.reasons);
                    refs.extend(r.refs);
                    supporting.extend(r.supporting);
                }
                if worst != CriterionStatus::Supported {
                    supporting.clear();
                }
                worst
            }
        }
        Evaluation::Human => match latest_on_subject(EvidenceCategory::HumanReview) {
            Some(e) => {
                refs.push(e.id.clone());
                let who = e
                    .actor
                    .as_ref()
                    .map(|a| a.id.as_str())
                    .unwrap_or("a reviewer");
                match e.outcome {
                    EvidenceOutcome::Passed => {
                        reasons.push(format!("Reviewed by {who} on this revision"));
                        CriterionStatus::Supported
                    }
                    EvidenceOutcome::Failed => {
                        reasons.push(format!("Rejected by {who} on this revision"));
                        CriterionStatus::Failed
                    }
                    EvidenceOutcome::Unknown => {
                        reasons.push("Requires your judgment".into());
                        CriterionStatus::NeedsJudgment
                    }
                }
            }
            None => {
                if on_other_subject(EvidenceCategory::HumanReview) {
                    reasons.push("Reviewed on an older revision; requires your judgment".into());
                } else {
                    reasons.push("Requires your judgment".into());
                }
                CriterionStatus::NeedsJudgment
            }
        },
        Evaluation::External => match latest_on_subject(EvidenceCategory::ExternalObservation) {
            Some(e) => {
                refs.push(e.id.clone());
                match e.outcome {
                    EvidenceOutcome::Passed => {
                        reasons.push(e.summary.clone().unwrap_or_else(|| {
                            "External outcome observed for this revision".into()
                        }));
                        CriterionStatus::Supported
                    }
                    EvidenceOutcome::Failed => {
                        reasons.push(
                            e.summary
                                .clone()
                                .unwrap_or_else(|| "External outcome does not match".into()),
                        );
                        CriterionStatus::Failed
                    }
                    EvidenceOutcome::Unknown => {
                        reasons.push("External lookup failed or offline; outcome unknown".into());
                        CriterionStatus::Unknown
                    }
                }
            }
            None => {
                if on_other_subject(EvidenceCategory::ExternalObservation) {
                    reasons.push(
                        "External observation is for an older revision; judge manually".into(),
                    );
                } else {
                    reasons
                        .push("External outcome not verified automatically; judge manually".into());
                }
                CriterionStatus::NeedsJudgment
            }
        },
    };

    refs.sort();
    refs.dedup();
    supporting.sort();
    supporting.dedup();
    CriterionAssessment {
        criterion_id: c.id.clone(),
        version: c.version,
        required: c.required,
        evaluation: c.evaluation,
        status,
        reasons,
        evidence_refs: refs,
        supporting_checks: supporting,
    }
}

fn live_blockers(live: &LiveState, out: &mut Vec<Blocker>) {
    for r in &live.bound_runs {
        match r.activity {
            RunActivity::Idle | RunActivity::Exited => {}
            a => out.push(Blocker::new(
                BlockerKind::RunActive,
                Some(&r.run_id),
                format!(
                    "Agent {} is {}",
                    r.run_id,
                    match a {
                        RunActivity::Working => "working",
                        RunActivity::Disconnected => "disconnected",
                        _ => "in an unknown state",
                    }
                ),
            )),
        }
    }
    for w in &live.known_writers {
        out.push(Blocker::new(
            BlockerKind::KnownWriter,
            Some(w),
            format!("Another writer is active in this checkout ({w})"),
        ));
    }
    for i in &live.open_interactions {
        out.push(Blocker::new(
            BlockerKind::OpenInteraction,
            Some(i),
            "An agent question or approval is open",
        ));
    }
    if live.pending_binding_switch {
        out.push(Blocker::new(
            BlockerKind::PendingBindingSwitch,
            None,
            "A task switch is pending for the bound run",
        ));
    }
    for m in &live.unresolved_deliveries {
        out.push(Blocker::new(
            BlockerKind::UnresolvedDelivery,
            Some(m),
            "A message to the agent has an unconfirmed delivery",
        ));
    }
    for c in &live.open_blocking_concerns {
        out.push(Blocker::new(
            BlockerKind::BlockingConcern,
            Some(c),
            "Unresolved blocking concern",
        ));
    }
}

/// Assess readiness (§7). `mappings_confirmed` = the user (or a trusted recipe) confirmed the
/// criterion/check and stop mappings for this intent revision.
pub fn assess(
    intent: Option<&TaskIntent>,
    subject: Option<&ChangeSubject>,
    evidence: &[Evidence],
    live: &LiveState,
    mappings_confirmed: bool,
) -> ReviewAssessment {
    let mut blockers = Vec::new();
    let mut explanation = Vec::new();

    let Some(intent) = intent else {
        blockers.push(Blocker::new(
            BlockerKind::IntentMissing,
            None,
            "No confirmed task intent",
        ));
        let label = if subject.is_some() || live.has_inspectable_changes {
            explanation.push("Changes to inspect; no requirement-completion claim".into());
            ReadinessLabel::ChangesToInspect
        } else {
            explanation.push("Turn finished; no statement about the task outcome".into());
            ReadinessLabel::TurnFinished
        };
        return ReviewAssessment {
            label,
            intent_revision: None,
            subject_id: subject.map(|s| s.id.clone()),
            criteria: vec![],
            blockers,
            explanation,
        };
    };

    let mut details_missing = false;
    if !mappings_confirmed {
        details_missing = true;
        blockers.push(Blocker::new(
            BlockerKind::MappingsUnconfirmed,
            None,
            "Criterion and check mappings are not confirmed",
        ));
    }
    if !intent.stop_at.is_confirmed() {
        details_missing = true;
        blockers.push(Blocker::new(
            BlockerKind::StopUnspecified,
            None,
            "Stopping point not specified",
        ));
    }

    let criteria: Vec<CriterionAssessment> = match subject {
        Some(s) => intent
            .criteria
            .iter()
            .map(|c| assess_criterion(c, s, evidence, live))
            .collect(),
        None => vec![],
    };

    match subject {
        None => blockers.push(Blocker::new(
            BlockerKind::NoSubject,
            None,
            "No captured change subject",
        )),
        Some(s) => {
            if !s.is_committed() {
                blockers.push(Blocker::new(
                    BlockerKind::SubjectNotCommitted,
                    Some(&s.id),
                    "Select a committed revision to record acceptance",
                ));
            }
            if !live.subject_is_current {
                blockers.push(Blocker::new(
                    BlockerKind::SubjectNotCurrent,
                    Some(&s.id),
                    format!(
                        "Revision {} is not the current candidate; older candidates stay inspectable",
                        short(&s.head_sha)
                    ),
                ));
            }
        }
    }
    if !live.sources_verified {
        blockers.push(Blocker::new(
            BlockerKind::SourcesUnverified,
            None,
            "Sources could not be revalidated (offline or incomplete history)",
        ));
    }
    for (a, c) in criteria.iter().zip(&intent.criteria) {
        if a.required && a.needs_exception() {
            blockers.push(Blocker::new(
                BlockerKind::RequiredCriterion,
                Some(&a.criterion_id),
                format!("{}: {}", a.status.text(), c.text),
            ));
        }
    }
    // Trusted check failures on this subject that no criterion maps are still known blocking
    // findings (§7).
    if let Some(s) = subject {
        let mapped: BTreeSet<&str> = intent
            .criteria
            .iter()
            .flat_map(|c| c.check_definition_ids.iter().map(String::as_str))
            .collect();
        let mut seen = BTreeSet::new();
        for e in evidence.iter().filter(|e| {
            e.category == EvidenceCategory::VibekeVerification
                && e.outcome == EvidenceOutcome::Failed
                && e.subject_id.as_deref() == Some(s.id.as_str())
        }) {
            let Some(d) = e.check_definition_id.as_deref() else {
                continue;
            };
            let current = live.check_definition_digests.get(d);
            if !mapped.contains(d)
                && current.is_some()
                && e.definition_digest.as_ref() == current
                && seen.insert(d)
            {
                blockers.push(Blocker::new(
                    BlockerKind::TrustedCheckFailed,
                    Some(d),
                    format!("Vibeke verification {d} failed on this revision"),
                ));
            }
        }
    }
    let before_live = blockers.len();
    live_blockers(live, &mut blockers);
    let live_activity = blockers[before_live..]
        .iter()
        .any(|b| matches!(b.kind, BlockerKind::RunActive | BlockerKind::KnownWriter));

    let fresh_acceptance = live
        .acceptance
        .as_ref()
        .filter(|a| a.task_id == intent.task_id);
    let label = if let Some(acc) = fresh_acceptance {
        let state = FreshnessState::from_live(intent, subject, live);
        let why = outdated_reasons(acc, &state);
        if why.is_empty() {
            explanation.push(acc.label().into());
            ReadinessLabel::Reviewed
        } else {
            explanation.push(format!(
                "Review outdated: {}",
                why.iter().map(|r| r.text()).collect::<Vec<_>>().join("; ")
            ));
            ReadinessLabel::ReviewOutdated
        }
    } else if details_missing {
        explanation.push("Needs task details: confirm criteria, checks and stopping point".into());
        ReadinessLabel::NeedsTaskDetails
    } else if subject.is_none() {
        if live.has_inspectable_changes {
            explanation.push("Changes to inspect; no candidate revision captured yet".into());
            ReadinessLabel::ChangesToInspect
        } else {
            explanation.push("Turn finished; no statement about the task outcome".into());
            ReadinessLabel::TurnFinished
        }
    } else if blockers.is_empty() {
        explanation.push(format!(
            "Ready for your review: required checks supported on revision {}",
            short(subject.map(|s| s.head_sha.as_str()).unwrap_or(""))
        ));
        ReadinessLabel::ReadyForReview
    } else {
        if live_activity || !live.sources_verified {
            explanation.push("Review available — agent/writer active or state unavailable".into());
        } else {
            explanation.push("Review available".into());
        }
        ReadinessLabel::ReviewAvailable
    };

    match intent.stop_at {
        StopAt::NoSeparateOutcome => explanation.push(
            "No separate delivery outcome to verify; final outcome needs your judgment".into(),
        ),
        StopAt::Unspecified => {}
        other => explanation.push(format!(
            "Stopping point: {}",
            serde_json::to_value(other)
                .ok()
                .and_then(|v| v.as_str().map(|s| s.replace('_', " ")))
                .unwrap_or_default()
        )),
    }
    explanation.extend(blockers.iter().map(|b| b.message.clone()));

    ReviewAssessment {
        label,
        intent_revision: Some(intent.revision),
        subject_id: subject.map(|s| s.id.clone()),
        criteria,
        blockers,
        explanation,
    }
}

/// A recorded exception for a criterion that is not supported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionException {
    pub criterion_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptRequest {
    pub task_id: String,
    pub expected_intent_revision: u32,
    pub expected_package_revision: u64,
    pub expected_subject_id: String,
    #[serde(default)]
    pub exceptions: Vec<CriterionException>,
    pub actor: Actor,
    pub idempotency_key: String,
}

/// The owner's current view at acceptance time (inside its state transaction).
#[derive(Debug, Clone, Copy)]
pub struct PackageSnapshot<'a> {
    pub task_id: &'a str,
    pub intent: &'a TaskIntent,
    pub package_revision: u64,
    pub subject: Option<&'a ChangeSubject>,
    pub assessment: &'a ReviewAssessment,
    pub sources_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAcceptance {
    pub id: String,
    pub task_id: String,
    pub intent_revision: u32,
    pub package_revision: u64,
    pub subject_id: String,
    pub head_sha: String,
    /// Check definition/environment identities that supported required criteria.
    #[serde(default)]
    pub checks: Vec<CheckKey>,
    #[serde(default)]
    pub exceptions: Vec<CriterionException>,
    pub actor: Actor,
    pub accepted_at_ms: i64,
    pub idempotency_key: String,
}

impl ReviewAcceptance {
    pub fn with_exceptions(&self) -> bool {
        !self.exceptions.is_empty()
    }
    pub fn label(&self) -> &'static str {
        if self.with_exceptions() {
            "Reviewed with exceptions"
        } else {
            "Reviewed"
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum AcceptConflict {
    /// `conflict reason=review_changed`: expected revisions/subject no longer match.
    #[error("review changed: {field} expected {expected}, current {actual}")]
    ReviewChanged {
        field: String,
        expected: String,
        actual: String,
    },
    #[error("no change subject to accept")]
    NoSubject,
    /// T2: formal acceptance requires a committed candidate.
    #[error("Select a committed revision to record acceptance")]
    SubjectNotCommitted,
    #[error("sources could not be revalidated; acceptance unavailable")]
    SourcesUnverified,
    #[error("only a user can accept a review")]
    NotAUser,
    #[error("required criteria need an explicit exception with a reason: {criteria:?}")]
    ExceptionsRequired { criteria: Vec<String> },
    #[error("invalid exception for {criterion_id}: {why}")]
    InvalidException { criterion_id: String, why: String },
}

impl AcceptConflict {
    pub fn reason(&self) -> &'static str {
        match self {
            AcceptConflict::ReviewChanged { .. } => "review_changed",
            AcceptConflict::NoSubject => "no_subject",
            AcceptConflict::SubjectNotCommitted => "subject_not_committed",
            AcceptConflict::SourcesUnverified => "sources_unverified",
            AcceptConflict::NotAUser => "not_a_user",
            AcceptConflict::ExceptionsRequired { .. } => "exceptions_required",
            AcceptConflict::InvalidException { .. } => "invalid_exception",
        }
    }
}

/// Validate and record **Mark reviewed** (§7). The caller serializes this with its state
/// transaction; on success it persists the returned acceptance.
pub fn accept(
    req: &AcceptRequest,
    current: &PackageSnapshot<'_>,
    now_ms: i64,
) -> Result<ReviewAcceptance, AcceptConflict> {
    if req.actor.kind != ActorKind::User {
        return Err(AcceptConflict::NotAUser);
    }
    let changed = |field: &str, expected: String, actual: String| AcceptConflict::ReviewChanged {
        field: field.into(),
        expected,
        actual,
    };
    if req.task_id != current.task_id {
        return Err(changed("task", req.task_id.clone(), current.task_id.into()));
    }
    if req.expected_intent_revision != current.intent.revision
        || current.assessment.intent_revision != Some(current.intent.revision)
    {
        return Err(changed(
            "intent_revision",
            req.expected_intent_revision.to_string(),
            current.intent.revision.to_string(),
        ));
    }
    if req.expected_package_revision != current.package_revision {
        return Err(changed(
            "package_revision",
            req.expected_package_revision.to_string(),
            current.package_revision.to_string(),
        ));
    }
    let subject = current.subject.ok_or(AcceptConflict::NoSubject)?;
    if req.expected_subject_id != subject.id
        || current.assessment.subject_id.as_deref() != Some(subject.id.as_str())
    {
        return Err(changed(
            "subject",
            req.expected_subject_id.clone(),
            subject.id.clone(),
        ));
    }
    if !subject.is_committed() || !subject.verify_id() {
        return Err(AcceptConflict::SubjectNotCommitted);
    }
    if !current.sources_verified {
        return Err(AcceptConflict::SourcesUnverified);
    }

    let mut excepted = BTreeSet::new();
    for ex in &req.exceptions {
        let Some(a) = current.assessment.criterion(&ex.criterion_id) else {
            return Err(AcceptConflict::InvalidException {
                criterion_id: ex.criterion_id.clone(),
                why: "unknown criterion".into(),
            });
        };
        if a.status == CriterionStatus::Supported {
            return Err(AcceptConflict::InvalidException {
                criterion_id: ex.criterion_id.clone(),
                why: "criterion is supported".into(),
            });
        }
        if ex.reason.trim().is_empty() {
            return Err(AcceptConflict::InvalidException {
                criterion_id: ex.criterion_id.clone(),
                why: "reason must not be empty".into(),
            });
        }
        if !excepted.insert(ex.criterion_id.as_str()) {
            return Err(AcceptConflict::InvalidException {
                criterion_id: ex.criterion_id.clone(),
                why: "duplicate exception".into(),
            });
        }
    }
    let missing: Vec<String> = current
        .assessment
        .criteria
        .iter()
        .filter(|a| {
            a.required && a.needs_exception() && !excepted.contains(a.criterion_id.as_str())
        })
        .map(|a| a.criterion_id.clone())
        .collect();
    if !missing.is_empty() {
        return Err(AcceptConflict::ExceptionsRequired { criteria: missing });
    }

    let mut checks: Vec<CheckKey> = current
        .assessment
        .criteria
        .iter()
        .filter(|a| a.required && a.status == CriterionStatus::Supported)
        .flat_map(|a| a.supporting_checks.iter().cloned())
        .collect();
    checks.sort();
    checks.dedup();

    Ok(ReviewAcceptance {
        id: new_id(),
        task_id: req.task_id.clone(),
        intent_revision: current.intent.revision,
        package_revision: current.package_revision,
        subject_id: subject.id.clone(),
        head_sha: subject.head_sha.clone(),
        checks,
        exceptions: req.exceptions.clone(),
        actor: req.actor.clone(),
        accepted_at_ms: now_ms,
        idempotency_key: req.idempotency_key.clone(),
    })
}

/// Current identities that an acceptance is compared against (§7 freshness).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FreshnessState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent_revision: Option<u32>,
    /// Current candidate subject; `None` = unavailable (outdated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    #[serde(default)]
    pub check_definition_digests: BTreeMap<String, String>,
    /// Current environment digest per check; a missing entry means unknown identity
    /// (→ `environment_unknown` for an acceptance that relied on that check).
    #[serde(default)]
    pub environment_digests: BTreeMap<String, String>,
    /// Checks whose environment identity is unavailable (→ `environment_unknown`).
    #[serde(default)]
    pub environment_unavailable: BTreeSet<String>,
    #[serde(default)]
    pub external_outcome_changed: bool,
}

impl FreshnessState {
    /// Build from an assessment's inputs. A subject that is not current counts as unavailable.
    pub fn from_live(
        intent: &TaskIntent,
        subject: Option<&ChangeSubject>,
        live: &LiveState,
    ) -> Self {
        FreshnessState {
            intent_revision: Some(intent.revision),
            subject_id: subject
                .filter(|_| live.subject_is_current)
                .map(|s| s.id.clone()),
            check_definition_digests: live.check_definition_digests.clone(),
            environment_digests: live.current_environment_digests.clone(),
            environment_unavailable: live.environment_unavailable.clone(),
            external_outcome_changed: live.external_outcome_changed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutdatedReason {
    IntentRevisionChanged { accepted: u32, current: Option<u32> },
    SubjectChanged,
    CheckDefinitionChanged { check_id: String },
    EnvironmentChanged { check_id: String },
    EnvironmentUnknown { check_id: String },
    ExternalOutcomeChanged,
}

impl OutdatedReason {
    pub fn text(&self) -> String {
        match self {
            OutdatedReason::IntentRevisionChanged { accepted, current } => format!(
                "task details changed (accepted revision {accepted}, now {})",
                current.map_or("unknown".into(), |c| c.to_string())
            ),
            OutdatedReason::SubjectChanged => "code changed since acceptance".into(),
            OutdatedReason::CheckDefinitionChanged { check_id } => {
                format!("check {check_id} definition changed")
            }
            OutdatedReason::EnvironmentChanged { check_id } => {
                format!("check {check_id} environment changed")
            }
            OutdatedReason::EnvironmentUnknown { check_id } => {
                format!("check {check_id} environment unknown")
            }
            OutdatedReason::ExternalOutcomeChanged => "external outcome changed".into(),
        }
    }
}

/// Why `acceptance` no longer matches `current` (empty = still current). A rebase changes the
/// subject id and so always invalidates; there is no semantic-equivalence shortcut.
pub fn outdated_reasons(
    acceptance: &ReviewAcceptance,
    current: &FreshnessState,
) -> Vec<OutdatedReason> {
    let mut out = Vec::new();
    if current.intent_revision != Some(acceptance.intent_revision) {
        out.push(OutdatedReason::IntentRevisionChanged {
            accepted: acceptance.intent_revision,
            current: current.intent_revision,
        });
    }
    if current.subject_id.as_deref() != Some(acceptance.subject_id.as_str()) {
        out.push(OutdatedReason::SubjectChanged);
    }
    for k in &acceptance.checks {
        if current.check_definition_digests.get(&k.check_definition_id)
            != Some(&k.definition_digest)
        {
            out.push(OutdatedReason::CheckDefinitionChanged {
                check_id: k.check_definition_id.clone(),
            });
        }
        let cur_env = current.environment_digests.get(&k.check_definition_id);
        if cur_env.is_none()
            || current
                .environment_unavailable
                .contains(&k.check_definition_id)
        {
            out.push(OutdatedReason::EnvironmentUnknown {
                check_id: k.check_definition_id.clone(),
            });
        } else if let Some(cur) = cur_env
            && k.environment_digest.as_ref() != Some(cur)
        {
            out.push(OutdatedReason::EnvironmentChanged {
                check_id: k.check_definition_id.clone(),
            });
        }
    }
    if current.external_outcome_changed {
        out.push(OutdatedReason::ExternalOutcomeChanged);
    }
    out
}

/// `true` when the acceptance no longer matches current state.
pub fn outdated(acceptance: &ReviewAcceptance, current: &FreshnessState) -> bool {
    !outdated_reasons(acceptance, current).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::{Constraint, Criterion};
    use crate::subject::{DirtyState, RepoIdentity, SubjectKind};

    const REDIRECT: &str = "redirect-test";
    const SSO: &str = "sso-test";
    const DIG: &str = "digest-1";
    const ENV: &str = "env-1";

    fn subject(head: &str) -> ChangeSubject {
        ChangeSubject::new(
            RepoIdentity {
                root: "/r".into(),
                origin_url: None,
            },
            "base".into(),
            head.into(),
            None,
            DirtyState::Clean,
            SubjectKind::Committed,
            0,
        )
    }

    fn criterion(id: &str, eval: Evaluation, checks: &[&str], required: bool) -> Criterion {
        Criterion {
            id: id.into(),
            version: 1,
            since_revision: 1,
            text: format!("criterion {id}"),
            required,
            evaluation: eval,
            check_definition_ids: checks.iter().map(|s| s.to_string()).collect(),
            source_refs: vec![],
        }
    }

    fn intent() -> TaskIntent {
        TaskIntent {
            task_id: "t1".into(),
            revision: 1,
            title: "Fix login redirect".into(),
            objective: "Return to original page".into(),
            constraints: vec![Constraint {
                id: "k1".into(),
                text: "Preserve SSO".into(),
                source_refs: vec![],
            }],
            criteria: vec![
                criterion("return", Evaluation::Check, &[REDIRECT], true),
                criterion("sso", Evaluation::Check, &[SSO], true),
                criterion("judge", Evaluation::Human, &[], true),
            ],
            stop_at: StopAt::DraftPr,
            stop_detail: None,
            source_refs: vec![],
            source_excerpt: None,
            source_excerpt_truncated: false,
            confirmed_by: Actor::user("demo"),
            confirmed_at_ms: 0,
        }
    }

    fn live() -> LiveState {
        LiveState {
            turn_finished: true,
            has_inspectable_changes: true,
            subject_is_current: true,
            sources_verified: true,
            bound_runs: vec![BoundRun {
                run_id: "run1".into(),
                activity: RunActivity::Idle,
            }],
            check_definition_digests: [
                (REDIRECT.to_string(), DIG.to_string()),
                (SSO.to_string(), DIG.to_string()),
            ]
            .into(),
            current_environment_digests: [
                (REDIRECT.to_string(), ENV.to_string()),
                (SSO.to_string(), ENV.to_string()),
            ]
            .into(),
            ..Default::default()
        }
    }

    fn verify(
        id: &str,
        check: &str,
        subject: &ChangeSubject,
        outcome: EvidenceOutcome,
    ) -> Evidence {
        Evidence {
            id: id.into(),
            category: EvidenceCategory::VibekeVerification,
            outcome,
            check_definition_id: Some(check.into()),
            definition_digest: Some(DIG.into()),
            environment_digest: Some(ENV.into()),
            subject_id: Some(subject.id.clone()),
            criterion_ids: vec![],
            actor: Some(Actor::user("demo")),
            observed_at_ms: 1,
            summary: None,
        }
    }

    fn all_pass(s: &ChangeSubject) -> Vec<Evidence> {
        vec![
            verify("e1", REDIRECT, s, EvidenceOutcome::Passed),
            verify("e2", SSO, s, EvidenceOutcome::Passed),
        ]
    }

    #[test]
    fn ready_when_everything_holds() {
        let s = subject("abc");
        let a = assess(Some(&intent()), Some(&s), &all_pass(&s), &live(), true);
        assert_eq!(a.label, ReadinessLabel::ReadyForReview, "{:#?}", a.blockers);
        assert!(a.blockers.is_empty());
        assert_eq!(
            a.criterion("return").unwrap().status,
            CriterionStatus::Supported
        );
        // Human criterion is presented for judgment, not blocking.
        assert_eq!(
            a.criterion("judge").unwrap().status,
            CriterionStatus::NeedsJudgment
        );
        assert!(a.explanation[0].starts_with("Ready for your review"));
    }

    #[test]
    fn claims_never_support() {
        // §12: "Agent says tests pass without observed execution".
        let s = subject("abc");
        let mut ev = vec![verify("e1", REDIRECT, &s, EvidenceOutcome::Passed)];
        ev.push(Evidence {
            id: "claim".into(),
            category: EvidenceCategory::AgentClaim,
            outcome: EvidenceOutcome::Passed,
            check_definition_id: Some(SSO.into()),
            definition_digest: Some(DIG.into()),
            environment_digest: None,
            subject_id: Some(s.id.clone()),
            criterion_ids: vec!["sso".into()],
            actor: Some(Actor::agent("run1")),
            observed_at_ms: 1,
            summary: Some("SSO still works".into()),
        });
        let a = assess(Some(&intent()), Some(&s), &ev, &live(), true);
        let sso = a.criterion("sso").unwrap();
        assert_eq!(sso.status, CriterionStatus::Missing);
        assert!(sso.reasons.iter().any(|r| r.contains("Agent claims")));
        assert_eq!(a.label, ReadinessLabel::ReviewAvailable);
    }

    #[test]
    fn unbound_observed_command_is_unknown() {
        let s = subject("abc");
        let mut ev = vec![verify("e1", REDIRECT, &s, EvidenceOutcome::Passed)];
        ev.push(Evidence {
            id: "obs".into(),
            category: EvidenceCategory::Observed,
            outcome: EvidenceOutcome::Passed,
            check_definition_id: Some(SSO.into()),
            definition_digest: Some(DIG.into()),
            environment_digest: None,
            subject_id: None,
            criterion_ids: vec![],
            actor: None,
            observed_at_ms: 1,
            summary: None,
        });
        let a = assess(Some(&intent()), Some(&s), &ev, &live(), true);
        let sso = a.criterion("sso").unwrap();
        assert_eq!(sso.status, CriterionStatus::Unknown);
        assert!(
            sso.reasons
                .iter()
                .any(|r| r.contains("Command passed · code binding unverified"))
        );
        assert_ne!(a.label, ReadinessLabel::ReadyForReview);
    }

    #[test]
    fn rebase_invalidates_sha_bound_evidence_and_acceptance() {
        let old = subject("abc");
        let rebased = subject("def");
        let a = assess(
            Some(&intent()),
            Some(&rebased),
            &all_pass(&old),
            &live(),
            true,
        );
        assert_eq!(
            a.criterion("return").unwrap().status,
            CriterionStatus::Stale
        );
        assert_eq!(a.label, ReadinessLabel::ReviewAvailable);

        // An acceptance of the old subject is outdated once the subject changes.
        let i = intent();
        let ok = assess(Some(&i), Some(&old), &all_pass(&old), &live(), true);
        let acc = accept(&req(&old, vec![]), &snap(&i, &old, &ok), 5).unwrap();
        let mut l = live();
        l.acceptance = Some(acc.clone());
        let now = assess(Some(&i), Some(&rebased), &all_pass(&rebased), &l, true);
        assert_eq!(now.label, ReadinessLabel::ReviewOutdated);
        assert!(outdated(
            &acc,
            &FreshnessState::from_live(&i, Some(&rebased), &l)
        ));
        let still = assess(Some(&i), Some(&old), &all_pass(&old), &l, true);
        assert_eq!(still.label, ReadinessLabel::Reviewed);
    }

    #[test]
    fn same_subject_red_green_needs_judgment_but_different_subjects_pass() {
        // §12: red then green on different subjects versus identical subject.
        let s1 = subject("abc");
        let s2 = subject("def");
        let mut ev = all_pass(&s2);
        ev.push(verify("old-fail", REDIRECT, &s1, EvidenceOutcome::Failed));
        let a = assess(Some(&intent()), Some(&s2), &ev, &live(), true);
        let ret = a.criterion("return").unwrap();
        assert_eq!(ret.status, CriterionStatus::Supported);
        assert!(ret.reasons.iter().any(|r| r.contains("history")));
        assert_eq!(a.label, ReadinessLabel::ReadyForReview);

        let mut ev = all_pass(&s2);
        ev.push(verify("flake", REDIRECT, &s2, EvidenceOutcome::Failed));
        let a = assess(Some(&intent()), Some(&s2), &ev, &live(), true);
        let ret = a.criterion("return").unwrap();
        assert_eq!(ret.status, CriterionStatus::NeedsJudgment);
        assert!(
            ret.reasons
                .iter()
                .any(|r| r.contains("retry does not erase"))
        );
        assert_ne!(a.label, ReadinessLabel::ReadyForReview);
    }

    #[test]
    fn environment_is_part_of_the_aggregation_key() {
        let s = subject("abc");
        let mut ev = all_pass(&s);
        let mut other_env = verify("e3", REDIRECT, &s, EvidenceOutcome::Failed);
        other_env.environment_digest = Some("env-linux".into());
        ev.push(other_env);
        // The current environment is env-1: a failure from another environment is neither a
        // flake of nor a pass for this one.
        let a = assess(Some(&intent()), Some(&s), &ev, &live(), true);
        assert_eq!(
            a.criterion("return").unwrap().status,
            CriterionStatus::Supported
        );
        // When that other environment is the current one, its failure is a real failure.
        let mut l = live();
        l.current_environment_digests
            .insert(REDIRECT.into(), "env-linux".into());
        let a = assess(Some(&intent()), Some(&s), &ev, &l, true);
        assert_eq!(
            a.criterion("return").unwrap().status,
            CriterionStatus::Failed
        );

        // With a current environment pinned, evidence from another environment is stale.
        let mut l = live();
        l.current_environment_digests
            .insert(SSO.into(), "env-2".into());
        let a = assess(Some(&intent()), Some(&s), &all_pass(&s), &l, true);
        assert_eq!(a.criterion("sso").unwrap().status, CriterionStatus::Stale);
    }

    #[test]
    fn missing_current_environment_identity_is_unknown_not_fresh() {
        let s = subject("abc");
        let mut l = live();
        l.current_environment_digests.remove(SSO);
        let a = assess(Some(&intent()), Some(&s), &all_pass(&s), &l, true);
        let sso = a.criterion("sso").unwrap();
        assert_eq!(sso.status, CriterionStatus::Unknown, "{sso:?}");
        assert!(sso.reasons.iter().any(|r| r.contains("freshness unknown")));
        assert_ne!(a.label, ReadinessLabel::ReadyForReview);
        // An acceptance that relied on that check has unknown freshness, too.
        let i = intent();
        let a = assess(Some(&i), Some(&s), &all_pass(&s), &live(), true);
        let acc = accept(&req(&s, vec![]), &snap(&i, &s, &a), 1).unwrap();
        let mut f = FreshnessState::from_live(&i, Some(&s), &live());
        f.environment_digests.remove(SSO);
        assert_eq!(
            outdated_reasons(&acc, &f),
            vec![OutdatedReason::EnvironmentUnknown {
                check_id: SSO.into()
            }]
        );
    }

    #[test]
    fn changed_definition_makes_evidence_stale() {
        let s = subject("abc");
        let mut l = live();
        l.check_definition_digests
            .insert(SSO.into(), "digest-2".into());
        let a = assess(Some(&intent()), Some(&s), &all_pass(&s), &l, true);
        let sso = a.criterion("sso").unwrap();
        assert_eq!(sso.status, CriterionStatus::Stale);
        assert!(sso.reasons[0].contains("definition"));
    }

    #[test]
    fn idle_after_failed_check_never_ready() {
        // §12: agent idle after a failed check or missing criterion.
        let s = subject("abc");
        let mut ev = vec![verify("e1", REDIRECT, &s, EvidenceOutcome::Failed)];
        ev.push(verify("e2", SSO, &s, EvidenceOutcome::Passed));
        let a = assess(Some(&intent()), Some(&s), &ev, &live(), true);
        assert_eq!(
            a.criterion("return").unwrap().status,
            CriterionStatus::Failed
        );
        assert_eq!(a.label, ReadinessLabel::ReviewAvailable);
        let missing = assess(
            Some(&intent()),
            Some(&s),
            &[verify("e2", SSO, &s, EvidenceOutcome::Passed)],
            &live(),
            true,
        );
        assert_eq!(
            missing.criterion("return").unwrap().status,
            CriterionStatus::Missing
        );
        assert_eq!(missing.label, ReadinessLabel::ReviewAvailable);
    }

    #[test]
    fn optional_failures_do_not_block() {
        let s = subject("abc");
        let mut i = intent();
        i.criteria
            .push(criterion("lint", Evaluation::Check, &["lint"], false));
        let mut l = live();
        l.check_definition_digests.insert("lint".into(), DIG.into());
        l.current_environment_digests
            .insert("lint".into(), ENV.into());
        let mut ev = all_pass(&s);
        ev.push(verify("lint1", "lint", &s, EvidenceOutcome::Failed));
        let a = assess(Some(&i), Some(&s), &ev, &l, true);
        assert_eq!(a.criterion("lint").unwrap().status, CriterionStatus::Failed);
        assert_eq!(a.label, ReadinessLabel::ReadyForReview);
    }

    #[test]
    fn unmapped_trusted_check_failure_blocks() {
        let s = subject("abc");
        let mut l = live();
        l.check_definition_digests.insert("e2e".into(), DIG.into());
        let mut ev = all_pass(&s);
        ev.push(verify("x", "e2e", &s, EvidenceOutcome::Failed));
        let a = assess(Some(&intent()), Some(&s), &ev, &l, true);
        assert!(
            a.blockers
                .iter()
                .any(|b| b.kind == BlockerKind::TrustedCheckFailed)
        );
        assert_eq!(a.label, ReadinessLabel::ReviewAvailable);
    }

    #[test]
    fn ready_unavailable_while_working_question_or_uncertain_send() {
        // §12: checks pass while a run works, question is open, or send is uncertain.
        let s = subject("abc");
        type Tweak = fn(&mut LiveState);
        let cases: &[(Tweak, BlockerKind)] = &[
            (
                |l| l.bound_runs[0].activity = RunActivity::Working,
                BlockerKind::RunActive,
            ),
            (
                |l| l.bound_runs[0].activity = RunActivity::Disconnected,
                BlockerKind::RunActive,
            ),
            (
                |l| l.open_interactions.push("q1".into()),
                BlockerKind::OpenInteraction,
            ),
            (
                |l| l.unresolved_deliveries.push("m1".into()),
                BlockerKind::UnresolvedDelivery,
            ),
            (
                |l| l.known_writers.push("shell:vim".into()),
                BlockerKind::KnownWriter,
            ),
            (
                |l| l.pending_binding_switch = true,
                BlockerKind::PendingBindingSwitch,
            ),
            (
                |l| l.open_blocking_concerns.push("c1".into()),
                BlockerKind::BlockingConcern,
            ),
            (
                |l| l.subject_is_current = false,
                BlockerKind::SubjectNotCurrent,
            ),
            (
                |l| l.sources_verified = false,
                BlockerKind::SourcesUnverified,
            ),
        ];
        for (tweak, kind) in cases {
            let mut l = live();
            tweak(&mut l);
            let a = assess(Some(&intent()), Some(&s), &all_pass(&s), &l, true);
            assert_eq!(a.label, ReadinessLabel::ReviewAvailable, "{kind:?}");
            assert!(a.blockers.iter().any(|b| b.kind == *kind), "{kind:?}");
        }
        let mut l = live();
        l.bound_runs[0].activity = RunActivity::Working;
        let a = assess(Some(&intent()), Some(&s), &all_pass(&s), &l, true);
        assert_eq!(
            a.explanation[0],
            "Review available — agent/writer active or state unavailable"
        );
    }

    #[test]
    fn needs_task_details_and_untracked_labels() {
        let s = subject("abc");
        let mut i = intent();
        i.stop_at = StopAt::Unspecified;
        let a = assess(Some(&i), Some(&s), &all_pass(&s), &live(), true);
        assert_eq!(a.label, ReadinessLabel::NeedsTaskDetails);
        assert!(
            a.blockers
                .iter()
                .any(|b| b.kind == BlockerKind::StopUnspecified)
        );
        let a = assess(Some(&intent()), Some(&s), &all_pass(&s), &live(), false);
        assert_eq!(a.label, ReadinessLabel::NeedsTaskDetails);
        // Explicit "no separate outcome" counts as confirmed.
        i.stop_at = StopAt::NoSeparateOutcome;
        let a = assess(Some(&i), Some(&s), &all_pass(&s), &live(), true);
        assert_eq!(a.label, ReadinessLabel::ReadyForReview);
        assert!(
            a.explanation
                .iter()
                .any(|e| e.contains("needs your judgment"))
        );

        // No intent: never a readiness claim.
        let a = assess(None, Some(&s), &all_pass(&s), &live(), true);
        assert_eq!(a.label, ReadinessLabel::ChangesToInspect);
        assert!(a.criteria.is_empty());
        let mut l = live();
        l.has_inspectable_changes = false;
        assert_eq!(
            assess(None, None, &[], &l, false).label,
            ReadinessLabel::TurnFinished
        );
        // Intent but no subject.
        assert_eq!(
            assess(Some(&intent()), None, &[], &live(), true).label,
            ReadinessLabel::ChangesToInspect
        );
    }

    #[test]
    fn dirty_live_subject_never_ready() {
        let mut s = subject("abc");
        s.kind = SubjectKind::CheckoutLive;
        let a = assess(Some(&intent()), Some(&s), &all_pass(&s), &live(), true);
        assert_eq!(a.label, ReadinessLabel::ReviewAvailable);
        assert!(
            a.blockers
                .iter()
                .any(|b| b.kind == BlockerKind::SubjectNotCommitted)
        );
    }

    #[test]
    fn human_and_external_criteria() {
        let s = subject("abc");
        let mut i = intent();
        i.criteria
            .push(criterion("pr", Evaluation::External, &[], true));
        let mut ev = all_pass(&s);
        ev.push(Evidence {
            id: "h1".into(),
            category: EvidenceCategory::HumanReview,
            outcome: EvidenceOutcome::Failed,
            check_definition_id: None,
            definition_digest: None,
            environment_digest: None,
            subject_id: Some(s.id.clone()),
            criterion_ids: vec!["judge".into()],
            actor: Some(Actor::user("demo")),
            observed_at_ms: 2,
            summary: None,
        });
        ev.push(Evidence {
            id: "pr1".into(),
            category: EvidenceCategory::ExternalObservation,
            outcome: EvidenceOutcome::Unknown,
            check_definition_id: None,
            definition_digest: None,
            environment_digest: None,
            subject_id: Some(s.id.clone()),
            criterion_ids: vec!["pr".into()],
            actor: None,
            observed_at_ms: 2,
            summary: None,
        });
        let a = assess(Some(&i), Some(&s), &ev, &live(), true);
        assert_eq!(
            a.criterion("judge").unwrap().status,
            CriterionStatus::Failed
        );
        assert_eq!(a.criterion("pr").unwrap().status, CriterionStatus::Unknown);
        assert_eq!(a.label, ReadinessLabel::ReviewAvailable);
        // Without an observation, external outcome is manual judgment and not blocking.
        let a = assess(Some(&i), Some(&s), &all_pass(&s), &live(), true);
        assert_eq!(
            a.criterion("pr").unwrap().status,
            CriterionStatus::NeedsJudgment
        );
        assert_eq!(a.label, ReadinessLabel::ReadyForReview);
    }

    fn req(s: &ChangeSubject, exceptions: Vec<CriterionException>) -> AcceptRequest {
        AcceptRequest {
            task_id: "t1".into(),
            expected_intent_revision: 1,
            expected_package_revision: 7,
            expected_subject_id: s.id.clone(),
            exceptions,
            actor: Actor::user("demo"),
            idempotency_key: "k1".into(),
        }
    }

    fn snap<'a>(
        i: &'a TaskIntent,
        s: &'a ChangeSubject,
        a: &'a ReviewAssessment,
    ) -> PackageSnapshot<'a> {
        PackageSnapshot {
            task_id: "t1",
            intent: i,
            package_revision: 7,
            subject: Some(s),
            assessment: a,
            sources_verified: true,
        }
    }

    #[test]
    fn accept_with_missing_check_requires_exception() {
        // §12: accept with missing required SSO check.
        let s = subject("abc");
        let i = intent();
        let a = assess(
            Some(&i),
            Some(&s),
            &[verify("e1", REDIRECT, &s, EvidenceOutcome::Passed)],
            &live(),
            true,
        );
        let err = accept(&req(&s, vec![]), &snap(&i, &s, &a), 1).unwrap_err();
        assert_eq!(
            err,
            AcceptConflict::ExceptionsRequired {
                criteria: vec!["sso".into()]
            }
        );
        let err = accept(
            &req(
                &s,
                vec![CriterionException {
                    criterion_id: "sso".into(),
                    reason: "  ".into(),
                }],
            ),
            &snap(&i, &s, &a),
            1,
        )
        .unwrap_err();
        assert_eq!(err.reason(), "invalid_exception");
        // Exception on a supported criterion is refused (no fabricated records).
        let err = accept(
            &req(
                &s,
                vec![CriterionException {
                    criterion_id: "return".into(),
                    reason: "x".into(),
                }],
            ),
            &snap(&i, &s, &a),
            1,
        )
        .unwrap_err();
        assert_eq!(err.reason(), "invalid_exception");
        let acc = accept(
            &req(
                &s,
                vec![CriterionException {
                    criterion_id: "sso".into(),
                    reason: "SSO verified manually on staging".into(),
                }],
            ),
            &snap(&i, &s, &a),
            1,
        )
        .unwrap();
        assert!(acc.with_exceptions());
        assert_eq!(acc.label(), "Reviewed with exceptions");
        assert_eq!(acc.subject_id, s.id);
        assert_eq!(acc.checks.len(), 1);
        // The assessment still shows the criterion as missing, never green.
        let mut l = live();
        l.acceptance = Some(acc);
        let after = assess(
            Some(&i),
            Some(&s),
            &[verify("e1", REDIRECT, &s, EvidenceOutcome::Passed)],
            &l,
            true,
        );
        assert_eq!(after.label, ReadinessLabel::Reviewed);
        assert_eq!(
            after.criterion("sso").unwrap().status,
            CriterionStatus::Missing
        );
        assert_eq!(after.explanation[0], "Reviewed with exceptions");
    }

    #[test]
    fn accept_conflicts() {
        let s = subject("abc");
        let i = intent();
        let a = assess(Some(&i), Some(&s), &all_pass(&s), &live(), true);
        let ok = snap(&i, &s, &a);
        assert!(accept(&req(&s, vec![]), &ok, 1).is_ok());

        let mut r = req(&s, vec![]);
        r.expected_intent_revision = 2;
        assert_eq!(accept(&r, &ok, 1).unwrap_err().reason(), "review_changed");
        let mut r = req(&s, vec![]);
        r.expected_package_revision = 6;
        assert_eq!(accept(&r, &ok, 1).unwrap_err().reason(), "review_changed");
        // Rebase/edit during acceptance: subject changed under the user.
        let moved = subject("def");
        assert_eq!(
            accept(&req(&moved, vec![]), &ok, 1).unwrap_err().reason(),
            "review_changed"
        );
        let mut r = req(&s, vec![]);
        r.actor = Actor::agent("run1");
        assert_eq!(accept(&r, &ok, 1).unwrap_err(), AcceptConflict::NotAUser);
        let mut gapped = ok;
        gapped.sources_verified = false;
        assert_eq!(
            accept(&req(&s, vec![]), &gapped, 1).unwrap_err(),
            AcceptConflict::SourcesUnverified
        );

        // T2: dirty/live subjects cannot be accepted.
        let mut live_s = subject("abc");
        live_s.kind = SubjectKind::CheckoutLive;
        live_s.id = ChangeSubject::compute_id(
            &live_s.repo,
            &live_s.base_sha,
            &live_s.head_sha,
            None,
            DirtyState::Clean,
            SubjectKind::CheckoutLive,
        );
        let a2 = assess(Some(&i), Some(&live_s), &all_pass(&live_s), &live(), true);
        assert_eq!(
            accept(&req(&live_s, vec![]), &snap(&i, &live_s, &a2), 1).unwrap_err(),
            AcceptConflict::SubjectNotCommitted
        );
    }

    #[test]
    fn freshness_rules() {
        let s = subject("abc");
        let i = intent();
        let a = assess(Some(&i), Some(&s), &all_pass(&s), &live(), true);
        let acc = accept(&req(&s, vec![]), &snap(&i, &s, &a), 1).unwrap();
        let fresh = FreshnessState::from_live(&i, Some(&s), &live());
        assert!(!outdated(&acc, &fresh));

        let mut f = fresh.clone();
        f.intent_revision = Some(2);
        assert!(outdated(&acc, &f));
        let mut f = fresh.clone();
        f.check_definition_digests
            .insert(SSO.into(), "changed".into());
        assert_eq!(
            outdated_reasons(&acc, &f),
            vec![OutdatedReason::CheckDefinitionChanged {
                check_id: SSO.into()
            }]
        );
        let mut f = fresh.clone();
        f.environment_digests.insert(SSO.into(), "other-env".into());
        assert!(outdated(&acc, &f));
        let mut f = fresh.clone();
        f.subject_id = None;
        assert!(outdated(&acc, &f));
        let mut f = fresh.clone();
        f.environment_unavailable.insert(REDIRECT.into());
        assert_eq!(
            outdated_reasons(&acc, &f),
            vec![OutdatedReason::EnvironmentUnknown {
                check_id: REDIRECT.into()
            }]
        );
        let mut f = fresh;
        f.external_outcome_changed = true;
        assert!(outdated(&acc, &f));
    }

    #[test]
    fn serde_shapes() {
        let s = subject("abc");
        let a = assess(Some(&intent()), Some(&s), &all_pass(&s), &live(), true);
        let j = serde_json::to_value(&a).unwrap();
        assert_eq!(j["label"], "ready_for_review");
        assert_eq!(j["criteria"][2]["status"], "needs_judgment");
        let back: ReviewAssessment = serde_json::from_value(j).unwrap();
        assert_eq!(back, a);
        let c = serde_json::to_value(AcceptConflict::NoSubject).unwrap();
        assert_eq!(c["reason"], "no_subject");
    }
}
