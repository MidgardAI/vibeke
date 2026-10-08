//! Requests from the server (`gateway.call`, spec 16 §15.5): the TUI asks its server to run
//! `peer.*` / `share.*` here, because the gateway state (keys, peers, invitations) lives in this
//! process. The server emits `gateway.request {id, method, params}` and waits for our
//! `gateway.reply`. Only the methods in [`ALLOWED`] run, through the same functions as the app
//! API, as the host's owner (`by: "tui"` in the audit log).

use std::sync::Arc;

use serde_json::{Value, json};

use crate::Gateway;
use crate::api::{ApiError, ApiResult};
use crate::state::{Device, Scope};

/// Must match the server's allow-list (`vk_server::gateway_bridge::ALLOWED`).
pub const ALLOWED: &[&str] = &[
    "peer.invite",
    "peer.redeem",
    "peer.list",
    "peer.remove",
    "share.create",
    "share.list",
    "share.revoke",
];

/// Who the audit log names for these requests.
pub const BY: &str = "tui";

fn s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(|v| v.as_str())
}

/// The host owner as a device: full scope, never stored.
fn owner() -> Device {
    Device {
        id: BY.into(),
        name: "Vibeke TUI".into(),
        platform: "tui".into(),
        public: String::new(),
        scope: Scope::Full,
        paired_at: 0,
        vapid_private: None,
        push: vec![],
        prefs: Default::default(),
        push_failures: 0,
        kind: "device".into(),
        expires_at: None,
        limit: None,
        peer: None,
    }
}

/// Answer server requests until the process stops.
pub async fn run(gw: Arc<Gateway>) {
    let mut rx = gw.hub.subscribe_requests();
    loop {
        match rx.recv().await {
            Ok(req) => {
                tokio::spawn(handle(gw.clone(), req));
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn handle(gw: Arc<Gateway>, req: Value) {
    let Some(id) = s(&req, "id").map(str::to_string) else {
        return;
    };
    let method = s(&req, "method").unwrap_or_default();
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    let body = match execute(&gw, method, &params).await {
        Ok(result) => json!({"id": id, "result": result}),
        Err(e) => {
            json!({"id": id, "error": {"kind": e.kind, "message": e.message, "details": e.details}})
        }
    };
    // A refusal means the request already timed out on the server: nothing to do.
    if let Err(e) = gw.server.call("gateway.reply", body).await {
        tracing::debug!("gateway.reply {id}: {}", e.message);
    }
}

/// Run one allowed request (checked again here; the server checked it first).
pub async fn execute(gw: &Arc<Gateway>, method: &str, p: &Value) -> ApiResult {
    if !ALLOWED.contains(&method) || !p.is_object() {
        return Err(ApiError::new(
            "forbidden",
            format!("{method} is not carried by the server bridge"),
        ));
    }
    let owner = owner();
    match method {
        "share.create" => match s(p, "kind") {
            Some("handoff") => crate::api::share_create_as(gw, &owner, p),
            Some("peer") => crate::peers::dispatch(gw, &owner, "peer.invite", p).await,
            _ => Err(ApiError::new(
                "forbidden",
                "the server bridge carries share.create for handoff and peer invitations only",
            )),
        },
        m => crate::peers::dispatch(gw, &owner, m, p).await,
    }
}
