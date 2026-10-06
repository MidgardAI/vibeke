//! Lane 2C, spec 15 §6.1 / §6.4: **recording a human review of a criterion**.
//!
//! `task.review.human_review {task, criterion, subject?, verdict, note?, screenshots?}` records
//! the user's judgment of one *human* criterion on one immutable subject (committed revision,
//! dirty snapshot or selected patch): `supported`, `failed`, or `withdrawn` (back to
//! **needs judgment**). It becomes `HumanReview` evidence with the actor and inspected subject,
//! which the readiness engine reads like any other evidence: it decides that criterion on that
//! subject only, an older subject's review shows as "reviewed on an older revision", and it
//! never touches check criteria. Screenshots named in the review must be bound to the reviewed
//! subject (running build = that code state, 06 B6); an illustrative screenshot is refused as
//! support. Full human-client scope only (15 §11).

use super::*;
use vk_review::Actor;
use vk_review::readiness::{EvidenceCategory, EvidenceOutcome};

pub(super) const K_HUMAN: &str = "review_human";

/// One recorded human judgment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanReviewRec {
    pub id: String,
    pub task: String,
    pub criterion_id: String,
    pub criterion_version: u32,
    pub intent_revision: u32,
    pub subject_id: String,
    /// `supported | failed | withdrawn`.
    pub verdict: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Bound screenshots the user inspected (ids).
    #[serde(default)]
    pub screenshots: Vec<String>,
    pub actor: Actor,
    pub at_ms: i64,
    /// Set when `forget` purged the note text (15 §11); the judgment itself stays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purged_at_ms: Option<i64>,
}

pub(super) fn reviews_of(c: &Core, task: &str) -> Vec<HumanReviewRec> {
    let mut v: Vec<HumanReviewRec> = by_task(c, K_HUMAN, task);
    v.sort_by(|a, b| (a.at_ms, &a.id).cmp(&(b.at_ms, &b.id)));
    v
}

/// The task's human reviews as readiness evidence.
pub(super) fn evidence(server: &Server, task: &str) -> Vec<Evidence> {
    server.with_core(|c| {
        reviews_of(c, task)
            .into_iter()
            .map(|r| Evidence {
                id: r.id.clone(),
                category: EvidenceCategory::HumanReview,
                outcome: match r.verdict.as_str() {
                    "supported" => EvidenceOutcome::Passed,
                    "failed" => EvidenceOutcome::Failed,
                    _ => EvidenceOutcome::Unknown,
                },
                check_definition_id: None,
                definition_digest: None,
                environment_digest: None,
                subject_id: Some(r.subject_id.clone()),
                criterion_ids: vec![r.criterion_id.clone()],
                actor: Some(r.actor.clone()),
                observed_at_ms: r.at_ms,
                summary: r.note.clone(),
            })
            .collect()
    })
}

/// For the review package (`human_reviews`).
pub(super) fn list_json(server: &Server, task: &str) -> Vec<Value> {
    server.with_core(|c| {
        reviews_of(c, task)
            .into_iter()
            .map(|r| {
                let mut v = serde_json::to_value(&r).unwrap_or(Value::Null);
                v["label"] = json!(match r.verdict.as_str() {
                    "supported" => "Reviewed by you · supports this criterion on this revision",
                    "failed" => "Reviewed by you · does not meet this criterion on this revision",
                    _ => "Review withdrawn · needs your judgment",
                });
                v
            })
            .collect()
    })
}

/// State-token contribution: a human review recorded after the package was shown is a known
/// competing update for acceptance.
pub(super) fn token_part(c: &Core, task: &str) -> String {
    let ids: Vec<String> = reviews_of(c, task)
        .into_iter()
        .map(|r| format!("{}:{}", r.id, r.verdict))
        .collect();
    format!("human {ids:?}\n")
}

/// `task.review.human_review {task, criterion, subject?, verdict, note?, screenshots?,
/// idempotency_key?}`.
pub(super) async fn human_review_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    const M: &str = "task.review.human_review";
    if caller_workspace(server, ctx)?.is_some() {
        return Err(err(
            ErrorKind::PermissionDenied,
            "recording a human review needs the user's client (15 §11)",
        ));
    }
    if let Some(r) = receipts::replay(server, ctx, M, p) {
        return r;
    }
    let task_id = req(p, "task")?;
    authorize_task(server, ctx, task_id)?;
    let criterion = req(p, "criterion")?.to_string();
    let verdict = req(p, "verdict")?;
    if !matches!(verdict, "supported" | "failed" | "withdrawn") {
        return Err(invalid("verdict: supported | failed | withdrawn"));
    }
    let note = s(p, "note").map(|n| n.chars().take(4000).collect::<String>());
    if verdict == "failed" && note.as_deref().is_none_or(|n| n.trim().is_empty()) {
        return Err(invalid("a failed review needs a note saying what is wrong"));
    }
    let shots: Vec<String> = p
        .get("screenshots")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let pkg = package(server, task_id, s(p, "subject")).await?;
    let intent = pkg
        .intent
        .clone()
        .ok_or_else(|| conflict("not_tracked", "task has no confirmed intent"))?;
    let crit = intent
        .criteria
        .iter()
        .find(|c| c.id == criterion)
        .ok_or_else(|| not_found("criterion", &criterion))?;
    if crit.evaluation != Evaluation::Human {
        return Err(conflict(
            "not_a_human_criterion",
            "only human criteria are decided by your review; check criteria need a Vibeke verification",
        )
        .details(json!({"reason": "not_a_human_criterion", "evaluation": crit.evaluation})));
    }
    let subj = pkg
        .selected
        .clone()
        .ok_or_else(|| conflict("no_subject", "there is no revision to review"))?;
    if !subj.is_immutable() {
        return Err(conflict(
            "subject_not_committed",
            "Select a committed revision or snapshot to record a review",
        ));
    }
    if let Some(want) = s(p, "expected_subject")
        && want != subj.id
    {
        return Err(review_changed(
            "the revision under review changed; inspect it again",
            json!({"field": "subject", "expected": want, "actual": subj.id}),
        ));
    }
    // Screenshots must be bound to exactly this subject to support the review.
    let rows = pkg.json["screenshots"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for id in &shots {
        let row = rows
            .iter()
            .find(|r| r["id"] == json!(id) || r["handle"] == json!(id))
            .ok_or_else(|| not_found("screenshot", id))?;
        if row["binding_here"] != "bound" {
            return Err(conflict(
                "screenshot_not_bound",
                "this screenshot is illustrative for the revision under review (build not verified or another revision); it cannot support the review",
            )
            .details(json!({"reason": "screenshot_not_bound", "screenshot": id, "note": row["note"]})));
        }
    }
    let rec = HumanReviewRec {
        id: format!("hr_{}", crate::core::ulid().to_lowercase()),
        task: pkg.task.id.clone(),
        criterion_id: crit.id.clone(),
        criterion_version: crit.version,
        intent_revision: intent.revision,
        subject_id: subj.id.clone(),
        verdict: verdict.to_string(),
        note,
        screenshots: shots,
        actor: tracking::user(ctx),
        at_ms: now(),
        purged_at_ms: None,
    };
    let result = json!({
        "review": rec,
        "note": "Your judgment decides this human criterion on this revision only; it is not a check result and does not accept the task.",
    });
    {
        let mut c = server.core.lock().unwrap();
        if let Some(r) = receipts::replay_in(&c, ctx, M, p) {
            return r;
        }
        let mut tx = Tx::new();
        tx.m.close(K_HUMAN, &rec.id, None, &rec);
        tx.event_by(
            "review.human_reviewed",
            json!({"task": rec.task, "criterion": rec.criterion_id, "subject": rec.subject_id}),
            json!({"kind": "user", "id": rec.actor.id}),
            json!({"verdict": rec.verdict, "screenshots": rec.screenshots.len(), "intent_revision": rec.intent_revision}),
        );
        receipts::record(&mut tx, ctx, M, p, &result);
        server.commit(&mut c, tx).map_err(internal)?;
    }
    spawn_refresh(server, &pkg.task.id);
    Ok(result)
}
