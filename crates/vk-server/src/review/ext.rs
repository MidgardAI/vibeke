//! Lane 2C (spec 15 extras) method table and dispatch: human review recording
//! ([`super::human`]), `forget` of derived review objects ([`super::purge`]), the Link run
//! status ([`super::link`]) and attention batches ([`super::attention_ext`]). Selected-patch
//! snapshots extend `task.review.snapshot` ([`super::patch`]); disposable reviewer checkouts
//! extend `task.review.start_reviewer` ([`super::scratch`]).

use super::*;

pub const METHODS: &[(&str, bool)] = &[
    ("task.review.human_review", true),
    ("task.review.forget", true),
    ("task.link.status", false),
    ("attention.batch", false),
];

/// Full human-client scope only (15 §11): read by `api::pane_scope_of`.
pub const PANE_FORBIDDEN: &[&str] = &["task.review.human_review", "task.review.forget"];

pub(super) async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "task.review.human_review" => human::human_review_api(server, ctx, p).await,
        "task.review.forget" => purge::forget_api(server, ctx, p).await,
        "task.link.status" => link::status_api(server, ctx, p),
        _ => return None,
    })
}
