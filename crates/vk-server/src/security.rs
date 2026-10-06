//! Server security surface (09, 07 §2.9): approval policy (`policy.*`), token revocation and
//! elevation (`auth.*`), the hash-chained audit log (`audit.*`) and integration tamper
//! detection (`integration.doctor`). Hooked into `api::dispatch` by [`authorize`] (before
//! dispatch: revoked panes, expired elevations) and [`api`]; capability refusals from
//! `api::authorize` are audited by [`denied`].

use crate::Server;
use crate::api::{Ctx, R};
use serde_json::{Value, json};
use std::sync::Arc;
use vk_proto::rpc::RpcError;

pub const METHODS: &[(&str, bool)] = &[
    ("policy.list", false),
    ("policy.add", true),
    ("policy.remove", true),
    ("policy.test", false),
    ("policy.trust", true),
    ("auth.revoke_token", true),
    ("auth.elevate", true),
    ("auth.elevate.decide", true),
    ("auth.list", false),
    ("audit.tail", false),
    ("audit.search", false),
    ("audit.verify", false),
    ("integration.doctor", false),
];

/// Methods of this module a pane-scoped caller may never call (09 §5.2: `policy.*`,
/// `integration.*`, revocation, the audit log). `auth.elevate` is meant for panes.
pub const PANE_FORBIDDEN: &[&str] = &[
    "policy.list",
    "policy.add",
    "policy.remove",
    "policy.test",
    "auth.revoke_token",
    "auth.elevate.decide",
    "auth.list",
    "audit.tail",
    "audit.search",
    "audit.verify",
    "integration.doctor",
];

#[derive(Default)]
pub struct State {
    pub audit: crate::audit::State,
    pub auth: crate::auth::State,
    pub integrity: crate::integrity::State,
}

/// Background tasks: the audit event flusher and the integration watcher.
pub fn start(server: &Arc<Server>) {
    crate::audit::start(server);
    crate::integrity::start(server);
}

/// Before dispatch (after `api::authorize`): revoked pane tokens and expired elevations.
pub fn authorize(server: &Server, ctx: &Ctx, method: &str) -> Result<(), RpcError> {
    crate::auth::authorize(server, ctx, method).inspect_err(|e| denied(server, ctx, method, e))
}

/// Audit a capability refusal (09 §11: capability violations, self-answer attempts).
pub fn denied(server: &Server, ctx: &Ctx, method: &str, e: &RpcError) {
    if e.data.kind != "permission_denied" {
        return;
    }
    let kind = if e.message.starts_with("self_answer_forbidden") {
        "security.self_answer_attempt"
    } else {
        "security.permission_denied"
    };
    crate::audit::record(
        server,
        kind,
        crate::audit::actor_of(ctx),
        json!({"method": method}),
        json!({"message": e.message}),
    );
}

/// Audit a recorded interaction decision (09 §11: who, channel, decision; auto-approvals by
/// policy).
pub fn decision_recorded(
    server: &Server,
    it: &vk_proto::model::Interaction,
    by: &str,
    actor: Option<&str>,
) {
    let kind = if by == "policy" {
        "policy.auto_answered"
    } else {
        "interaction.answered"
    };
    crate::audit::record(
        server,
        kind,
        json!({"kind": by, "actor": actor}),
        json!({"interaction": it.id, "pane": it.pane, "run": it.run}),
        json!({"decision": it.answer.as_ref().and_then(|a| a.decision).map(|d| format!("{d:?}").to_lowercase()),
               "channel": format!("{:?}", it.answer_channel).to_lowercase(),
               "kind": it.kind.as_str(), "source": format!("{:?}", it.source).to_lowercase(),
               "tool": it.action.as_ref().map(|a| a.tool.clone()),
               "rev": it.decision_rev}),
    );
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    let prefix = method.split('.').next().unwrap_or("");
    match prefix {
        "policy" => crate::policy_api::api(server, ctx, method, p),
        "auth" => crate::auth::api(server, ctx, method, p).await,
        "audit" => crate::audit::api(server, ctx, method, p),
        "integration" => crate::integrity::api(server, ctx, method, p),
        _ => None,
    }
}
