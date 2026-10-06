//! Confirmed task intent (§4.1) and communication coverage (§2.3).
//!
//! A [`TaskIntent`] is an immutable, confirmed revision. Edits are made on an [`IntentDraft`]
//! (never authority) and turned into the next revision with [`next_revision`], which keeps
//! criterion ids stable and bumps a criterion's `version` only when its meaning changed.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{Actor, ActorKind, SourceRef, new_id, truncate_utf8};

/// Upper bound on the stored user-selected source excerpt (§4.1).
pub const MAX_SOURCE_EXCERPT_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evaluation {
    /// Machine-verifiable through one or more check definitions.
    Check,
    /// Requires human judgment of the inspected subject.
    Human,
    /// An external outcome (PR state, deployment); manual/unknown unless observed.
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopAt {
    /// Not mapped yet. Never satisfies readiness.
    #[default]
    Unspecified,
    Implementation,
    DraftPr,
    ReviewedPr,
    Merge,
    VerifiedDeployment,
    Custom,
    /// The user explicitly confirmed "No separate delivery outcome to verify"; the final outcome
    /// assessment then requires human judgment.
    NoSeparateOutcome,
}

impl StopAt {
    /// The stopping point has been explicitly mapped by the user.
    pub fn is_confirmed(self) -> bool {
        self != StopAt::Unspecified
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub source_refs: Vec<SourceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Criterion {
    /// Stable across revisions.
    pub id: String,
    /// Bumped when `text`, `required` or `evaluation` changes.
    pub version: u32,
    /// Intent revision in which this `version` was introduced.
    pub since_revision: u32,
    pub text: String,
    pub required: bool,
    pub evaluation: Evaluation,
    /// Confirmed check mappings (user or trusted recipe). Changing them does not change the
    /// criterion's meaning, so it does not bump `version`.
    #[serde(default)]
    pub check_definition_ids: Vec<String>,
    #[serde(default)]
    pub source_refs: Vec<SourceRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskIntent {
    pub task_id: String,
    pub revision: u32,
    pub title: String,
    pub objective: String,
    #[serde(default)]
    pub constraints: Vec<Constraint>,
    #[serde(default)]
    pub criteria: Vec<Criterion>,
    pub stop_at: StopAt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_detail: Option<String>,
    #[serde(default)]
    pub source_refs: Vec<SourceRef>,
    /// Bounded (≤ [`MAX_SOURCE_EXCERPT_BYTES`]) copy of the user-selected source text. `None`
    /// when none was selected or after an explicit purge; never restored from a cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_excerpt: Option<String>,
    #[serde(default)]
    pub source_excerpt_truncated: bool,
    pub confirmed_by: Actor,
    pub confirmed_at_ms: i64,
}

impl TaskIntent {
    pub fn criterion(&self, id: &str) -> Option<&Criterion> {
        self.criteria.iter().find(|c| c.id == id)
    }
    pub fn required_criteria(&self) -> impl Iterator<Item = &Criterion> {
        self.criteria.iter().filter(|c| c.required)
    }
}

/// A draft constraint. `id` names an existing constraint to keep its identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftConstraint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub text: String,
    #[serde(default)]
    pub source_refs: Vec<SourceRef>,
}

/// A draft criterion. `id` names an existing criterion to keep its identity; `None` creates a
/// new criterion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftCriterion {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub text: String,
    pub required: bool,
    pub evaluation: Evaluation,
    #[serde(default)]
    pub check_definition_ids: Vec<String>,
    #[serde(default)]
    pub source_refs: Vec<SourceRef>,
}

/// Editable, non-authoritative intent (§10.1 "drafts stored separately").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentDraft {
    pub task_id: String,
    pub title: String,
    pub objective: String,
    #[serde(default)]
    pub constraints: Vec<DraftConstraint>,
    #[serde(default)]
    pub criteria: Vec<DraftCriterion>,
    #[serde(default)]
    pub stop_at: StopAt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_detail: Option<String>,
    #[serde(default)]
    pub source_refs: Vec<SourceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_excerpt: Option<String>,
}

impl IntentDraft {
    /// A draft that edits `intent` (ids preserved).
    pub fn from_intent(intent: &TaskIntent) -> Self {
        IntentDraft {
            task_id: intent.task_id.clone(),
            title: intent.title.clone(),
            objective: intent.objective.clone(),
            constraints: intent
                .constraints
                .iter()
                .map(|c| DraftConstraint {
                    id: Some(c.id.clone()),
                    text: c.text.clone(),
                    source_refs: c.source_refs.clone(),
                })
                .collect(),
            criteria: intent
                .criteria
                .iter()
                .map(|c| DraftCriterion {
                    id: Some(c.id.clone()),
                    text: c.text.clone(),
                    required: c.required,
                    evaluation: c.evaluation,
                    check_definition_ids: c.check_definition_ids.clone(),
                    source_refs: c.source_refs.clone(),
                })
                .collect(),
            stop_at: intent.stop_at,
            stop_detail: intent.stop_detail.clone(),
            source_refs: intent.source_refs.clone(),
            source_excerpt: intent.source_excerpt.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntentError {
    #[error("draft is for task {draft} but the previous revision belongs to {prev}")]
    TaskMismatch { prev: String, draft: String },
    #[error("duplicate id {0} in draft")]
    DuplicateId(String),
    #[error("only a user can confirm task intent (got {0:?})")]
    NotAUser(ActorKind),
    #[error("title must not be empty")]
    EmptyTitle,
}

/// Build the next immutable intent revision from a confirmed draft.
///
/// - Revision is `prev.revision + 1`, or 1.
/// - A draft criterion whose `id` matches a previous criterion keeps that id. Its `version`
///   (and `since_revision`) changes only when `text`, `required` or `evaluation` changed.
/// - A draft criterion with an unknown `id` keeps the supplied id as a new criterion
///   (version 1); `None` mints a new id.
/// - The excerpt is truncated to [`MAX_SOURCE_EXCERPT_BYTES`] on a char boundary.
/// - Only [`ActorKind::User`] may confirm (§11: agents cannot confirm intent).
pub fn next_revision(
    prev: Option<&TaskIntent>,
    draft: IntentDraft,
    actor: Actor,
    now_ms: i64,
) -> Result<TaskIntent, IntentError> {
    if actor.kind != ActorKind::User {
        return Err(IntentError::NotAUser(actor.kind));
    }
    if draft.title.trim().is_empty() {
        return Err(IntentError::EmptyTitle);
    }
    if let Some(p) = prev
        && p.task_id != draft.task_id
    {
        return Err(IntentError::TaskMismatch {
            prev: p.task_id.clone(),
            draft: draft.task_id.clone(),
        });
    }
    let revision = prev.map_or(1, |p| p.revision + 1);

    let mut seen = BTreeSet::new();
    for id in draft
        .criteria
        .iter()
        .filter_map(|c| c.id.as_ref())
        .chain(draft.constraints.iter().filter_map(|c| c.id.as_ref()))
    {
        if !seen.insert(id.clone()) {
            return Err(IntentError::DuplicateId(id.clone()));
        }
    }

    let prev_criteria: BTreeMap<&str, &Criterion> = prev
        .map(|p| p.criteria.iter().map(|c| (c.id.as_str(), c)).collect())
        .unwrap_or_default();

    let criteria = draft
        .criteria
        .into_iter()
        .map(|d| {
            let old =
                d.id.as_deref()
                    .and_then(|id| prev_criteria.get(id).copied());
            let (id, version, since_revision) = match old {
                Some(o) => {
                    let same = o.text == d.text
                        && o.required == d.required
                        && o.evaluation == d.evaluation;
                    if same {
                        (o.id.clone(), o.version, o.since_revision)
                    } else {
                        (o.id.clone(), o.version + 1, revision)
                    }
                }
                None => (
                    d.id.unwrap_or_else(|| format!("crit-{}", new_id())),
                    1,
                    revision,
                ),
            };
            Criterion {
                id,
                version,
                since_revision,
                text: d.text,
                required: d.required,
                evaluation: d.evaluation,
                check_definition_ids: d.check_definition_ids,
                source_refs: d.source_refs,
            }
        })
        .collect();

    let constraints = draft
        .constraints
        .into_iter()
        .map(|d| Constraint {
            id: d.id.unwrap_or_else(|| format!("cons-{}", new_id())),
            text: d.text,
            source_refs: d.source_refs,
        })
        .collect();

    let (source_excerpt, source_excerpt_truncated) = match draft.source_excerpt {
        None => (None, false),
        Some(s) => {
            let (t, cut) = truncate_utf8(&s, MAX_SOURCE_EXCERPT_BYTES);
            (Some(t.to_string()), cut)
        }
    };

    Ok(TaskIntent {
        task_id: draft.task_id,
        revision,
        title: draft.title,
        objective: draft.objective,
        constraints,
        criteria,
        stop_at: draft.stop_at,
        stop_detail: draft.stop_detail,
        source_refs: draft.source_refs,
        source_excerpt,
        source_excerpt_truncated,
        confirmed_by: actor,
        confirmed_at_ms: now_ms,
    })
}

/// How a criterion version reached the agent's conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "via", rename_all = "snake_case")]
pub enum CommunicationVia {
    /// A `TaskMessage` whose delivery reached `delivered` (not `delivery_unknown`).
    DeliveredMessage { message_id: String },
    /// The user linked an instruction already sent to this conversation.
    LinkedInstruction { source: SourceRef },
}

/// Communication coverage for one criterion version in one native conversation (§2.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommunicationRecord {
    pub criterion_id: String,
    pub version: u32,
    pub native_conversation_id: String,
    #[serde(flatten)]
    pub via: CommunicationVia,
}

/// Criterion ids whose current version has not been communicated to `native_conversation_id`.
///
/// A criterion counts as communicated when a [`CommunicationRecord`] names its exact id, version
/// and this conversation, or — for requirements introduced in revision 1 — when one of its
/// source refs is a user message already delivered to this exact conversation. Manually added
/// requirements (no such source ref), later versions and records for other conversations do not
/// count. Optional criteria are included too: the label is about what the agent was told.
pub fn uncommunicated(
    intent: &TaskIntent,
    coverage: &[CommunicationRecord],
    native_conversation_id: &str,
) -> Vec<String> {
    intent
        .criteria
        .iter()
        .filter(|c| {
            let quoted = c.since_revision == 1
                && c.version == 1
                && c.source_refs.iter().any(|r| {
                    r.delivered_user_message
                        && r.native_conversation_id.as_deref() == Some(native_conversation_id)
                });
            let recorded = coverage.iter().any(|r| {
                r.criterion_id == c.id
                    && r.version == c.version
                    && r.native_conversation_id == native_conversation_id
            });
            !(quoted || recorded)
        })
        .map(|c| c.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> Actor {
        Actor::user("demo")
    }

    fn quoted(conv: &str) -> SourceRef {
        SourceRef {
            run: Some("run1".into()),
            native_conversation_id: Some(conv.into()),
            turn: Some(1),
            delivered_user_message: true,
            ..Default::default()
        }
    }

    fn draft() -> IntentDraft {
        IntentDraft {
            task_id: "t1".into(),
            title: "Fix login redirect".into(),
            objective: "Return users to their original page".into(),
            constraints: vec![DraftConstraint {
                id: None,
                text: "Preserve SSO".into(),
                source_refs: vec![quoted("c1")],
            }],
            criteria: vec![
                DraftCriterion {
                    id: None,
                    text: "Return to original page".into(),
                    required: true,
                    evaluation: Evaluation::Check,
                    check_definition_ids: vec!["redirect-test".into()],
                    source_refs: vec![quoted("c1")],
                },
                DraftCriterion {
                    id: None,
                    text: "Preserve SSO".into(),
                    required: true,
                    evaluation: Evaluation::Human,
                    check_definition_ids: vec![],
                    source_refs: vec![],
                },
            ],
            stop_at: StopAt::Unspecified,
            stop_detail: None,
            source_refs: vec![quoted("c1")],
            source_excerpt: Some("Fix the login redirect. Draft PR only.".into()),
        }
    }

    #[test]
    fn first_revision_mints_ids_and_starts_at_one() {
        let i = next_revision(None, draft(), user(), 10).unwrap();
        assert_eq!(i.revision, 1);
        assert!(
            i.criteria
                .iter()
                .all(|c| c.version == 1 && c.since_revision == 1)
        );
        assert_ne!(i.criteria[0].id, i.criteria[1].id);
        assert_eq!(i.stop_at, StopAt::Unspecified);
        assert!(!i.stop_at.is_confirmed());
        assert_eq!(i.confirmed_at_ms, 10);
    }

    #[test]
    fn ids_stable_and_version_bumps_only_on_meaning_change() {
        let r1 = next_revision(None, draft(), user(), 1).unwrap();
        let mut d = IntentDraft::from_intent(&r1);
        // Mapping change only: no version bump.
        d.criteria[0].check_definition_ids.push("e2e".into());
        // Meaning change: bump.
        d.criteria[1].required = false;
        d.stop_at = StopAt::DraftPr;
        let r2 = next_revision(Some(&r1), d, user(), 2).unwrap();
        assert_eq!(r2.revision, 2);
        assert_eq!(r2.criteria[0].id, r1.criteria[0].id);
        assert_eq!(r2.criteria[0].version, 1);
        assert_eq!(r2.criteria[0].since_revision, 1);
        assert_eq!(r2.criteria[1].id, r1.criteria[1].id);
        assert_eq!(r2.criteria[1].version, 2);
        assert_eq!(r2.criteria[1].since_revision, 2);
        assert_eq!(r2.constraints[0].id, r1.constraints[0].id);

        let mut d = IntentDraft::from_intent(&r2);
        d.criteria[0].text = "Return to original page, or dashboard if gone".into();
        d.criteria.push(DraftCriterion {
            id: None,
            text: "Expired SSO session".into(),
            required: true,
            evaluation: Evaluation::Check,
            check_definition_ids: vec![],
            source_refs: vec![],
        });
        let r3 = next_revision(Some(&r2), d, user(), 3).unwrap();
        assert_eq!(r3.criteria[0].version, 2);
        assert_eq!(r3.criteria[2].version, 1);
        assert_eq!(r3.criteria[2].since_revision, 3);
        // r2 is untouched (immutable revisions).
        assert_eq!(r2.criteria[0].version, 1);
    }

    #[test]
    fn excerpt_is_bounded_on_char_boundary() {
        let mut d = draft();
        d.source_excerpt = Some("é".repeat(MAX_SOURCE_EXCERPT_BYTES));
        let i = next_revision(None, d, user(), 1).unwrap();
        let ex = i.source_excerpt.unwrap();
        assert!(ex.len() <= MAX_SOURCE_EXCERPT_BYTES);
        assert!(i.source_excerpt_truncated);
        assert!(ex.chars().all(|c| c == 'é'));
    }

    #[test]
    fn agents_cannot_confirm_and_duplicates_rejected() {
        assert_eq!(
            next_revision(None, draft(), Actor::agent("run1"), 1),
            Err(IntentError::NotAUser(ActorKind::Agent))
        );
        let mut d = draft();
        d.criteria[0].id = Some("x".into());
        d.criteria[1].id = Some("x".into());
        assert_eq!(
            next_revision(None, d, user(), 1),
            Err(IntentError::DuplicateId("x".into()))
        );
        let r1 = next_revision(None, draft(), user(), 1).unwrap();
        let mut d = draft();
        d.task_id = "other".into();
        assert!(matches!(
            next_revision(Some(&r1), d, user(), 1),
            Err(IntentError::TaskMismatch { .. })
        ));
    }

    #[test]
    fn quoted_revision_one_requirements_count_as_communicated() {
        let r1 = next_revision(None, draft(), user(), 1).unwrap();
        // Criterion 0 is quoted from a delivered message in c1; criterion 1 was added manually.
        assert_eq!(
            uncommunicated(&r1, &[], "c1"),
            vec![r1.criteria[1].id.clone()]
        );
        // A different conversation (e.g. after /clear) has been told nothing.
        assert_eq!(uncommunicated(&r1, &[], "c2").len(), 2);
    }

    #[test]
    fn changed_version_needs_new_communication() {
        let r1 = next_revision(None, draft(), user(), 1).unwrap();
        let mut d = IntentDraft::from_intent(&r1);
        d.criteria[0].text = "Changed meaning".into();
        let r2 = next_revision(Some(&r1), d, user(), 2).unwrap();
        let id0 = r2.criteria[0].id.clone();
        let id1 = r2.criteria[1].id.clone();
        assert_eq!(
            uncommunicated(&r2, &[], "c1"),
            vec![id0.clone(), id1.clone()]
        );
        let coverage = vec![
            CommunicationRecord {
                criterion_id: id0.clone(),
                version: 1, // stale version
                native_conversation_id: "c1".into(),
                via: CommunicationVia::DeliveredMessage {
                    message_id: "m1".into(),
                },
            },
            CommunicationRecord {
                criterion_id: id1.clone(),
                version: 1,
                native_conversation_id: "c1".into(),
                via: CommunicationVia::DeliveredMessage {
                    message_id: "m2".into(),
                },
            },
        ];
        assert_eq!(uncommunicated(&r2, &coverage, "c1"), vec![id0.clone()]);
        let mut coverage = coverage;
        coverage.push(CommunicationRecord {
            criterion_id: id0,
            version: 2,
            native_conversation_id: "c1".into(),
            via: CommunicationVia::LinkedInstruction {
                source: SourceRef::default(),
            },
        });
        assert!(uncommunicated(&r2, &coverage, "c1").is_empty());
    }

    #[test]
    fn serde_round_trip_snake_case() {
        let i = next_revision(None, draft(), user(), 1).unwrap();
        let j = serde_json::to_value(&i).unwrap();
        assert_eq!(j["stop_at"], "unspecified");
        assert_eq!(j["criteria"][0]["evaluation"], "check");
        let back: TaskIntent = serde_json::from_value(j).unwrap();
        assert_eq!(back, i);
        let rec = CommunicationRecord {
            criterion_id: "c".into(),
            version: 1,
            native_conversation_id: "n".into(),
            via: CommunicationVia::DeliveredMessage {
                message_id: "m".into(),
            },
        };
        let j = serde_json::to_value(&rec).unwrap();
        assert_eq!(j["via"], "delivered_message");
        assert_eq!(
            serde_json::from_value::<CommunicationRecord>(j).unwrap(),
            rec
        );
    }
}
