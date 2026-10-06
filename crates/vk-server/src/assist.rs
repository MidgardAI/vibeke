//! `assistant.*` (spec 14; 15 §2.2, §10.2, §11): user-invoked LLM drafts.
//!
//! Lifecycle: `assistant.generate` gathers only the selected inputs (after consent is checked,
//! before anything is retrieved), redacts and bounds them, and returns the **exact payload** as
//! a preview in state `awaiting_confirmation`. Nothing is sent until `assistant.confirm` names
//! that preview's digest (or the operation is on both the config's and the workspace consent's
//! `auto_send` lists). Confirmed requests go `queued -> running -> done | failed | cancelled`;
//! unfinished requests become `interrupted` after a restart and are never replayed.
//!
//! Boundaries: pane-scoped callers get no assistant access; the only Vibeke methods an
//! operation can call are the read-only ones in [`READ_ONLY`]; generated output is stored as a
//! validated draft and never interpreted as an action. Nothing here is triggered by turn
//! events, agent launches or timers — every provider call starts from an explicit request.
//! Events carry metadata only (operation, provider, model, token counts, cost, state).

use crate::Server;
use crate::api::{Ctx, R, b, err, invalid, not_found, req, s, u};
use crate::core::Tx;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use vk_assist::budget::{Amount, Ledger, RateWindow};
use vk_assist::config::{AssistConfig, Resolved};
use vk_assist::context::{Limits, Package, Payload, Source, SourceInput};
use vk_assist::ops::{self, Operation};
use vk_assist::provider::Usage;
use vk_assist::{AssistError, Category, clip};
use vk_proto::model::*;
use vk_proto::rpc::{ErrorKind, RpcError};

pub const METHODS: &[(&str, bool)] = &[
    ("assistant.status", false),
    ("assistant.providers", false),
    ("assistant.consent", true),
    ("assistant.revoke", true),
    ("assistant.generate", true),
    ("assistant.confirm", true),
    ("assistant.get", false),
    ("assistant.list", false),
    ("assistant.cancel", true),
    ("assistant.purge", true),
];

/// Vibeke methods an operation may call while gathering context. Everything else — in
/// particular every mutation, `interaction.answer`, `task.message.send`, `task.check.run` —
/// is refused on the operation path.
pub const READ_ONLY: &[&str] = &["task.review.get", "task.intent.get", "pane.read"];

const K_REQ: &str = "assist_request";
const KV_SCOPE: &str = "assistant";
const KV_LEDGER: &str = "ledger";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReqState {
    AwaitingConfirmation,
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
    Interrupted,
}

impl ReqState {
    fn open(self) -> bool {
        matches!(
            self,
            ReqState::AwaitingConfirmation | ReqState::Queued | ReqState::Running
        )
    }
    fn as_str(self) -> &'static str {
        match self {
            ReqState::AwaitingConfirmation => "awaiting_confirmation",
            ReqState::Queued => "queued",
            ReqState::Running => "running",
            ReqState::Done => "done",
            ReqState::Failed => "failed",
            ReqState::Cancelled => "cancelled",
            ReqState::Interrupted => "interrupted",
        }
    }
}

/// The stored request. Holds source metadata and the validated draft, never the payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistRequest {
    pub id: String,
    /// Constant marker so every record (open or closed) can be listed in one query.
    pub record: String,
    pub operation: String,
    pub state: ReqState,
    pub workspace: String,
    pub workspace_path: String,
    pub inputs: Value,
    pub profile: String,
    pub connection: String,
    pub adapter: String,
    pub model: String,
    pub endpoint_host: String,
    pub machine: String,
    pub prompt_version: String,
    pub sources: Vec<Source>,
    pub omitted: Vec<String>,
    pub redactions: usize,
    pub redaction_notice: String,
    pub context_digest: String,
    pub preview_digest: String,
    pub payload_bytes: usize,
    pub estimated_input_tokens: u64,
    pub estimation_method: String,
    pub max_output_tokens: u64,
    pub auto_sent: bool,
    pub usage: Usage,
    pub attempts: u32,
    pub estimated_cost_usd: Option<f64>,
    pub finish_reason: Option<String>,
    pub error: Option<AssistError>,
    pub output: Option<Value>,
    pub created_by: String,
    pub idempotency_key: Option<String>,
    pub retry_of: Option<String>,
    pub created_at_ms: i64,
    pub queued_at_ms: Option<i64>,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

struct Prepared {
    payload: Payload,
    resolved: Resolved,
    source_ids: Vec<String>,
    targets: Vec<String>,
    expires: Instant,
}

#[derive(Default)]
pub struct State {
    prepared: Mutex<HashMap<String, Prepared>>,
    running: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    reserved: Mutex<HashMap<String, Amount>>,
    rate: Mutex<RateWindow>,
    ledger: Mutex<()>,
    sem: OnceLock<Arc<Semaphore>>,
    recovered: OnceLock<()>,
}

fn now() -> i64 {
    vk_store::now_ms()
}

// ---- errors -------------------------------------------------------------------------------------

pub fn rpc(e: AssistError) -> RpcError {
    let kind = match e.category {
        Category::Disabled | Category::NotConfigured | Category::UnsupportedCapability => {
            ErrorKind::Unsupported
        }
        Category::PermissionDenied | Category::AuthenticationFailed => ErrorKind::PermissionDenied,
        Category::ContextTooLarge => ErrorKind::InvalidParams,
        Category::QueueFull | Category::BudgetExhausted | Category::RateLimited => {
            ErrorKind::RateLimited
        }
        Category::ProviderUnavailable => ErrorKind::RemoteUnavailable,
        Category::InvalidOutput => ErrorKind::Internal,
        Category::Timeout => ErrorKind::Timeout,
        Category::Cancelled | Category::Interrupted => ErrorKind::Conflict,
    };
    let reason = e
        .message
        .split_once(':')
        .map(|(r, _)| r)
        .filter(|r| !r.contains(' '))
        .map(str::to_string);
    err(kind, e.message.clone()).details(json!({"category": e.category, "reason": reason}))
}

fn ae(c: Category, m: impl Into<String>) -> RpcError {
    rpc(AssistError::new(c, m))
}

// ---- configuration ------------------------------------------------------------------------------

pub fn load_config() -> Result<(AssistConfig, Vec<String>), String> {
    let cfg = match vk_config::Config::load(vk_config::config_path()) {
        Ok((c, _)) => c,
        Err(e) => return Err(format!("config.toml: {e}")),
    };
    let table = cfg
        .extra
        .get("assistant")
        .or_else(|| cfg.extra.get("assist"))
        .and_then(|t| serde_json::to_value(t).ok());
    let a = match table {
        Some(v) => AssistConfig::from_json(v)?,
        None => AssistConfig::default(),
    };
    let patterns: Vec<String> = cfg
        .extra
        .get("security")
        .and_then(|t| serde_json::to_value(t).ok())
        .and_then(|v| serde_json::from_value(v["redact"]["patterns"].clone()).ok())
        .unwrap_or_default();
    Ok((a, patterns))
}

fn config() -> Result<(AssistConfig, Vec<String>), RpcError> {
    load_config().map_err(|e| ae(Category::NotConfigured, e))
}

fn enabled_config() -> Result<(AssistConfig, Vec<String>), RpcError> {
    let (c, p) = config()?;
    if !c.enabled {
        return Err(ae(
            Category::Disabled,
            "assistance is disabled; set [assistant] enabled = true in your user config",
        ));
    }
    Ok((c, p))
}

pub fn consent_path() -> PathBuf {
    crate::paths::state_root().join("assistant-consent.json")
}

fn canonical(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

// ---- API ----------------------------------------------------------------------------------------

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if !method.starts_with("assistant.") {
        return None;
    }
    // 14 §9: pane/adapter tokens receive no assistant access by default.
    if ctx.pane_scope.is_some() {
        return Some(Err(err(
            ErrorKind::PermissionDenied,
            format!("{method} is not available to pane-scoped callers"),
        )
        .details(json!({"scope": "pane", "category": "permission_denied"}))));
    }
    maintain(server);
    Some(match method {
        "assistant.status" => status(server),
        "assistant.providers" => providers(),
        "assistant.consent" => consent(server, ctx, p),
        "assistant.revoke" => revoke(server, ctx, p),
        "assistant.generate" => generate(server, ctx, p).await,
        "assistant.confirm" => confirm(server, p),
        "assistant.get" => get(server, p),
        "assistant.list" => list(server, p),
        "assistant.cancel" => cancel(server, p),
        "assistant.purge" => purge(server, p),
        _ => Err(err(
            ErrorKind::MethodNotFound,
            format!("unknown method {method}"),
        )),
    })
}

fn state(server: &Server) -> &State {
    &server.assist
}

fn all_requests(server: &Server) -> Vec<AssistRequest> {
    server.with_core(|c| {
        c.store
            .load_by_field(K_REQ, "$.record", "assist")
            .unwrap_or_default()
    })
}

fn find(server: &Server, id: &str) -> Result<AssistRequest, RpcError> {
    server
        .with_core(|c| c.store.find::<AssistRequest>(K_REQ, id).ok().flatten())
        .ok_or_else(|| not_found("assistant request", id))
}

fn put(tx: &mut Tx, r: &AssistRequest) {
    if r.state.open() {
        tx.m.put(K_REQ, &r.id, None, r);
    } else {
        tx.m.close(K_REQ, &r.id, None, r);
    }
}

/// Metadata-only event data (never prompts, sources' text or generated output).
fn meta(r: &AssistRequest) -> Value {
    json!({
        "operation": r.operation,
        "state": r.state.as_str(),
        "adapter": r.adapter,
        "connection": r.connection,
        "model": r.model,
        "endpoint_host": r.endpoint_host,
        "workspace": r.workspace,
        "sources": r.sources.len(),
        "redactions": r.redactions,
        "payload_bytes": r.payload_bytes,
        "input_tokens": r.usage.input_tokens,
        "output_tokens": r.usage.output_tokens,
        "attempts": r.attempts,
        "estimated_cost_usd": r.estimated_cost_usd,
        "error_category": r.error.as_ref().map(|e| e.category),
        "auto_sent": r.auto_sent,
    })
}

fn save(server: &Server, r: &AssistRequest, event: Option<&str>) -> Result<(), RpcError> {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    put(&mut tx, r);
    if let Some(kind) = event {
        tx.event_by(
            kind,
            json!({"assistant_request": r.id}),
            json!({"kind": "system", "client": r.created_by}),
            meta(r),
        );
    }
    server.commit(&mut c, tx).map_err(crate::api::internal)?;
    Ok(())
}

fn view(r: &AssistRequest, with_output: bool) -> Value {
    let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("record");
        if !with_output {
            o.remove("output");
        }
        o.insert(
            "label".into(),
            json!(
                Operation::parse(&r.operation)
                    .map(|op| op.label())
                    .unwrap_or("")
            ),
        );
    }
    v
}

// ---- maintenance: restart recovery, retention, disable ------------------------------------------

fn maintain(server: &Arc<Server>) {
    let st = state(server);
    let first = st.recovered.set(()).is_ok();
    let cfg = load_config().map(|(c, _)| c).unwrap_or_default();
    let retention = cfg.result_retention_hours as i64 * 3_600_000;
    let t = now();
    let mut changed: Vec<(AssistRequest, &'static str)> = vec![];
    let mut purge: Vec<String> = vec![];
    for mut r in all_requests(server) {
        if first && r.state.open() {
            // Unfinished at startup: never replayed automatically (14 §8).
            let (st2, cat) = if r.state == ReqState::AwaitingConfirmation {
                (ReqState::Cancelled, Category::Cancelled)
            } else {
                (ReqState::Interrupted, Category::Interrupted)
            };
            r.state = st2;
            r.error = Some(AssistError::new(
                cat,
                "the server restarted before this request finished",
            ));
            r.finished_at_ms = Some(t);
            changed.push((r, "assistant.request_finished"));
            continue;
        }
        if !cfg.enabled && r.state.open() {
            // 14 §10: disabling assistance cancels queued/running work.
            if let Some(h) = st.running.lock().unwrap().remove(&r.id) {
                h.abort();
            }
            st.prepared.lock().unwrap().remove(&r.id);
            release(server, &r.id, r.state == ReqState::Running, None);
            r.state = ReqState::Cancelled;
            r.error = Some(AssistError::new(
                Category::Disabled,
                "assistance was disabled",
            ));
            r.finished_at_ms = Some(t);
            changed.push((r, "assistant.request_finished"));
            continue;
        }
        if !r.state.open() && r.finished_at_ms.is_some_and(|f| t - f > retention) {
            purge.push(r.id);
        }
    }
    if changed.is_empty() && purge.is_empty() {
        return;
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    for (r, kind) in &changed {
        put(&mut tx, r);
        tx.event_by(
            kind,
            json!({"assistant_request": r.id}),
            json!({"kind": "system"}),
            meta(r),
        );
    }
    for id in &purge {
        tx.m.delete(K_REQ, id);
    }
    if !purge.is_empty() {
        tx.event(
            "assistant.purged",
            json!({}),
            json!({"count": purge.len(), "reason": "retention"}),
        );
    }
    let _ = server.commit(&mut c, tx);
}

// ---- budget ledger ------------------------------------------------------------------------------

fn ledger(server: &Server) -> Ledger {
    let mut l: Ledger = server
        .with_core(|c| c.store.kv_get(KV_SCOPE, KV_LEDGER).ok().flatten())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    l.roll(now());
    l
}

fn store_ledger(server: &Server, l: &Ledger) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.kv(
        KV_SCOPE,
        KV_LEDGER,
        Some(serde_json::to_string(l).unwrap_or_default()),
    );
    let _ = server.commit(&mut c, tx);
}

fn reserved_total(st: &State) -> Amount {
    let mut t = Amount::default();
    for a in st.reserved.lock().unwrap().values() {
        t.add(*a);
    }
    t
}

/// Drop a reservation; with `consumed`, record actual usage (or the reservation when the
/// provider's usage is unknown — e.g. cancelled mid-request).
fn release(server: &Server, id: &str, consumed: bool, actual: Option<Amount>) {
    let st = state(server);
    let _g = st.ledger.lock().unwrap();
    let res = st.reserved.lock().unwrap().remove(id);
    if !consumed {
        return;
    }
    let Some(res) = res else {
        return;
    };
    let mut l = ledger(server);
    l.used.add(actual.unwrap_or(res));
    store_ledger(server, &l);
}

// ---- status / providers / consent ---------------------------------------------------------------

fn status(server: &Server) -> R {
    let st = state(server);
    let (cfg, _) = match load_config() {
        Ok(c) => c,
        Err(e) => {
            return Ok(json!({"enabled": false, "configured": false, "config_error": e}));
        }
    };
    let resolved = cfg.resolve(None);
    let reqs = all_requests(server);
    let count = |s: ReqState| reqs.iter().filter(|r| r.state == s).count();
    let l = ledger(server);
    let grants = vk_assist::consent::load(&consent_path());
    Ok(json!({
        "enabled": cfg.enabled,
        "configured": resolved.is_ok(),
        "config_problem": resolved.as_ref().err().map(|e| e.message.clone()),
        "coordinator": {"machine": server.opts.machine, "session": server.opts.session},
        "profile": resolved.as_ref().ok().map(|r| json!({
            "id": r.profile_id,
            "connection": r.connection_id,
            "adapter": r.connection.adapter,
            "model": r.profile.model,
            "endpoint_host": r.endpoint_host(),
            "credential": vk_assist::config::credential_source(&r.connection),
            "max_input_tokens": r.profile.max_input_tokens,
            "max_input_bytes": r.profile.max_input_bytes,
            "max_output_tokens": r.profile.max_output_tokens,
            "pricing_usd_per_mtok": r.prices().map(|(i, o)| json!({"input": i, "output": o})),
        })),
        "limits": {
            "daily_request_limit": cfg.daily_request_limit,
            "daily_token_limit": cfg.daily_token_limit,
            "daily_cost_limit_usd": cfg.daily_cost_limit_usd,
            "requests_per_minute": cfg.requests_per_minute,
            "max_concurrent_requests": cfg.max_concurrent_requests,
            "max_queued_requests": cfg.max_queued_requests,
            "request_timeout_seconds": cfg.request_timeout_seconds,
            "result_retention_hours": cfg.result_retention_hours,
        },
        "today": {"utc_day": l.day, "used": l.used, "reserved": reserved_total(st), "remaining": l.remaining(&cfg, reserved_total(st))},
        "auto_send": cfg.auto_send,
        "consents": grants,
        "requests": {
            "awaiting_confirmation": count(ReqState::AwaitingConfirmation),
            "queued": count(ReqState::Queued),
            "running": count(ReqState::Running),
            "stored": reqs.len(),
        },
        "operations": ops::ALL.iter().map(|o| json!({"name": o.as_str(), "classes": o.classes(), "label": o.label()})).collect::<Vec<_>>(),
        "background": false,
    }))
}

fn providers() -> R {
    let (cfg, _) = config()?;
    let conns: Vec<Value> = cfg
        .connections
        .iter()
        .map(|(id, c)| {
            let endpoint = c
                .endpoint
                .clone()
                .unwrap_or_else(|| c.adapter.default_endpoint().into());
            json!({
                "id": id,
                "adapter": c.adapter,
                "endpoint": vk_assist::config::validate_endpoint(&endpoint).ok(),
                "endpoint_error": vk_assist::config::validate_endpoint(&endpoint).err().map(|e| e.message),
                "credential": vk_assist::config::credential_source(c),
                "verified": "unverified: exercised only against a local fake server",
            })
        })
        .collect();
    let profiles: Vec<Value> = cfg
        .profiles
        .iter()
        .map(|(id, p)| json!({"id": id, "connection": p.connection, "model": p.model}))
        .collect();
    Ok(json!({"connections": conns, "profiles": profiles, "default_profile": cfg.default_profile}))
}

fn list_param(p: &Value, k: &str) -> Option<Vec<String>> {
    match p.get(k)? {
        Value::Array(a) => Some(
            a.iter()
                .filter_map(Value::as_str)
                .flat_map(|s| s.split(','))
                .map(|s| s.trim().replace('-', "_"))
                .filter(|s| !s.is_empty())
                .collect(),
        ),
        Value::String(s) => Some(
            s.split(',')
                .map(|s| s.trim().replace('-', "_"))
                .filter(|s| !s.is_empty())
                .collect(),
        ),
        _ => None,
    }
}

fn consent(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let (cfg, _) = config()?;
    let ws = crate::api::resolve_ws(server, ctx, s(p, "workspace"))?;
    let resolved = match s(p, "connection") {
        Some(c) => {
            // Resolve through any profile on this connection to validate the endpoint.
            let pid = cfg
                .profiles
                .iter()
                .find(|(_, pr)| pr.connection == c)
                .map(|(id, _)| id.clone())
                .ok_or_else(|| {
                    ae(
                        Category::NotConfigured,
                        format!("no profile uses connection `{c}`"),
                    )
                })?;
            cfg.resolve(Some(&pid)).map_err(rpc)?
        }
        None => cfg.resolve(s(p, "profile")).map_err(rpc)?,
    };
    let classes = list_param(p, "classes")
        .unwrap_or_else(|| ops::DEFAULT_CLASSES.iter().map(|s| s.to_string()).collect());
    for c in &classes {
        if !ops::CLASSES.contains(&c.as_str()) {
            return Err(invalid(format!(
                "unknown context class `{c}` (known: {})",
                ops::CLASSES.join(", ")
            )));
        }
    }
    let operations = list_param(p, "operations").unwrap_or_default();
    let auto_send = list_param(p, "auto_send").unwrap_or_default();
    for o in operations.iter().chain(&auto_send) {
        if Operation::parse(o).is_none() {
            return Err(invalid(format!("unknown operation `{o}`")));
        }
    }
    let g = vk_assist::consent::Grant {
        workspace: canonical(&ws.root_path),
        connection: resolved.connection_id.clone(),
        fingerprint: resolved.fingerprint.clone(),
        adapter: resolved.connection.adapter.as_str().into(),
        endpoint_host: resolved.endpoint_host(),
        operations,
        classes,
        auto_send,
        granted_at_ms: now(),
        granted_by: ctx.client_id.clone(),
    };
    let g = vk_assist::consent::grant(&consent_path(), g).map_err(crate::api::internal)?;
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event_by(
        "assistant.consent_granted",
        json!({"workspace": ws.id}),
        json!({"kind": "user", "client": ctx.client_id}),
        json!({"connection": g.connection, "adapter": g.adapter, "endpoint_host": g.endpoint_host,
               "classes": g.classes, "operations": g.operations, "auto_send": g.auto_send}),
    );
    let _ = server.commit(&mut c, tx);
    Ok(json!({
        "consent": g,
        "notice": "Selected content from this workspace may be sent to the configured provider when you confirm a request. Pattern redaction is applied but cannot guarantee removal of all sensitive content. Revoking stops future requests; it cannot retract content already sent.",
    }))
}

fn revoke(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let ws = crate::api::resolve_ws(server, ctx, s(p, "workspace"))?;
    let path = canonical(&ws.root_path);
    let gone = vk_assist::consent::revoke(&consent_path(), &path, s(p, "connection"))
        .map_err(crate::api::internal)?;
    // Revocation cancels this workspace's unfinished requests.
    let mut cancelled = 0;
    for r in all_requests(server) {
        if r.state.open()
            && r.workspace_path == path
            && s(p, "connection").is_none_or(|c| c == r.connection)
        {
            let _ = cancel_one(server, r, Category::PermissionDenied, "consent revoked");
            cancelled += 1;
        }
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event_by(
        "assistant.consent_revoked",
        json!({"workspace": ws.id}),
        json!({"kind": "user", "client": ctx.client_id}),
        json!({"grants": gone.len(), "cancelled": cancelled}),
    );
    let _ = server.commit(&mut c, tx);
    Ok(json!({"revoked": gone.len(), "cancelled_requests": cancelled}))
}

// ---- read-only gateway --------------------------------------------------------------------------

/// The operation path's only access to other Vibeke methods. Refuses anything not in
/// [`READ_ONLY`] (09 §5, 15 §10.2: generated outputs and operations cannot mutate).
pub fn check_read_only(method: &str) -> Result<(), RpcError> {
    if READ_ONLY.contains(&method) {
        return Ok(());
    }
    Err(err(
        ErrorKind::PermissionDenied,
        format!("assistant_read_only: {method} cannot be called from an assistant operation"),
    )
    .details(json!({"category": "permission_denied", "reason": "assistant_read_only"})))
}

pub async fn read_call(server: &Arc<Server>, ctx: &Ctx, method: &str, params: Value) -> R {
    check_read_only(method)?;
    let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
    let resp: Value =
        serde_json::from_str(&Box::pin(crate::api::handle_line(server, ctx, &line)).await)
            .map_err(crate::api::internal)?;
    if let Some(e) = resp.get("error") {
        return Err(serde_json::from_value(e.clone())
            .unwrap_or_else(|_| err(ErrorKind::Internal, e.to_string())));
    }
    Ok(resp["result"].clone())
}

// ---- targets and context gathering --------------------------------------------------------------

struct Target {
    ws: Workspace,
    run: Option<AgentRun>,
    task: Option<Task>,
    pane: Option<Pane>,
    turns: Vec<u32>,
    include_screen: bool,
}

fn input<'a>(p: &'a Value, k: &str) -> Option<&'a Value> {
    p.get("inputs").and_then(|i| i.get(k)).or_else(|| p.get(k))
}
fn input_s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    input(p, k).and_then(Value::as_str)
}

fn ws_of_pane(server: &Server, pane: &str) -> Option<Workspace> {
    server.with_core(|c| c.pane(pane).and_then(|p| c.ws(&p.workspace).cloned()))
}

fn resolve_target(
    server: &Arc<Server>,
    ctx: &Ctx,
    op: Operation,
    p: &Value,
) -> Result<Target, RpcError> {
    let turns: Vec<u32> = match input(p, "turns").or_else(|| input(p, "turn")) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u32))
            .collect(),
        Some(Value::Number(n)) => n.as_u64().map(|n| vec![n as u32]).unwrap_or_default(),
        Some(Value::String(s)) => s
            .split(',')
            .filter_map(|x| x.trim().rsplit(':').next()?.parse().ok())
            .collect(),
        _ => vec![],
    };
    let include_screen = input(p, "include_screen").and_then(Value::as_bool) == Some(true);
    let run = match input_s(p, "run") {
        Some(r) => Some(
            server
                .with_core(|c| c.run(r).cloned())
                .ok_or_else(|| not_found("run", r))?,
        ),
        None => None,
    };
    let task = match input_s(p, "task") {
        Some(t) => Some(crate::tracking::find_task(server, t)?),
        None => None,
    };
    let pane = match input_s(p, "pane") {
        Some(t) => Some(crate::api::resolve_pane(server, ctx, Some(t))?),
        None => None,
    };
    let (run, pane) = match (run, pane) {
        (None, Some(pn)) => (
            server.with_core(|c| c.run_for_pane(&pn.id).cloned()),
            Some(pn),
        ),
        (Some(r), None) => {
            let pn = server.with_core(|c| c.pane(&r.pane).cloned());
            (Some(r), pn)
        }
        x => x,
    };
    let ws = if let Some(w) = input_s(p, "workspace").or_else(|| {
        p.get("scope")
            .and_then(|s| s.get("workspace"))
            .and_then(Value::as_str)
    }) {
        crate::api::resolve_ws(server, ctx, Some(w))?
    } else if let Some(pn) = &pane {
        ws_of_pane(server, &pn.id).ok_or_else(|| not_found("workspace", &pn.workspace))?
    } else if let Some(t) = &task {
        let wid = t
            .workspace
            .clone()
            .or_else(|| {
                crate::tracking::task_bindings(server, &t.id)
                    .last()
                    .and_then(|b| {
                        server.with_core(|c| {
                            c.run(&b.run_id)
                                .and_then(|r| c.pane(&r.pane))
                                .map(|p| p.workspace.clone())
                        })
                    })
            })
            .ok_or_else(|| invalid("the task has no workspace to check consent against"))?;
        crate::api::resolve_ws(server, ctx, Some(&wid))?
    } else {
        crate::api::resolve_ws(server, ctx, None)?
    };
    let need = |what: &str| invalid(format!("{} needs {what}", op.as_str()));
    match op {
        Operation::SuggestTaskDetails if run.is_none() => {
            return Err(need("--run or --pane (the selected request)"));
        }
        Operation::PaneTitle if pane.is_none() => return Err(need("--pane")),
        Operation::ReviewSummary | Operation::EffortEstimate if task.is_none() => {
            return Err(need("--task"));
        }
        Operation::Handoff if task.is_none() && run.is_none() => {
            return Err(need("--task or --run"));
        }
        _ => {}
    }
    Ok(Target {
        ws,
        run,
        task,
        pane,
        turns,
        include_screen,
    })
}

fn src(
    kind: &str,
    object: Value,
    label: impl Into<String>,
    text: impl Into<String>,
    at: Option<i64>,
) -> SourceInput {
    SourceInput {
        kind: kind.into(),
        object,
        label: label.into(),
        text: text.into(),
        observed_at_ms: at,
    }
}

/// Drop bulky or out-of-class fields from a review package before it becomes a source.
fn prune(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|k, _| {
                !matches!(
                    k.as_str(),
                    "diff"
                        | "patch"
                        | "log"
                        | "logs"
                        | "stdout"
                        | "stderr"
                        | "output"
                        | "env"
                        | "screenshot_path"
                        | "path_on_disk"
                )
            });
            for x in m.values_mut() {
                prune(x);
            }
        }
        Value::Array(a) => {
            a.truncate(40);
            for x in a {
                prune(x);
            }
        }
        _ => {}
    }
}

async fn review_source(server: &Arc<Server>, ctx: &Ctx, task: &Task) -> Vec<SourceInput> {
    let mut out = vec![];
    if let Ok(i) = read_call(server, ctx, "task.intent.get", json!({"task": task.id})).await
        && !i["intent"].is_null()
    {
        out.push(src(
            "intent",
            json!({"task": task.id, "revision": i["intent"]["revision"]}),
            format!("confirmed intent of {}", task.handle),
            serde_json::to_string_pretty(&i["intent"]).unwrap_or_default(),
            None,
        ));
    }
    match read_call(server, ctx, "task.review.get", json!({"task": task.id})).await {
        Ok(mut pkg) => {
            prune(&mut pkg);
            out.push(src(
                "review_package",
                json!({"task": task.id, "revision": pkg.get("package_revision").or(pkg.get("revision")).cloned()}),
                format!("review package of {}", task.handle),
                serde_json::to_string_pretty(&pkg).unwrap_or_default(),
                Some(now()),
            ));
        }
        Err(e) => out.push(src(
            "review_package",
            json!({"task": task.id}),
            "review package unavailable",
            format!("The review package could not be built: {}", e.message),
            Some(now()),
        )),
    }
    out
}

fn turn_sources(
    server: &Server,
    run: &AgentRun,
    select: &[u32],
    latest: usize,
) -> Vec<SourceInput> {
    let turns = crate::tracking::turns_of(server, &run.id, 50);
    let chosen: Vec<_> = if select.is_empty() {
        turns.into_iter().take(latest).collect()
    } else {
        turns
            .into_iter()
            .filter(|t| select.contains(&t.n))
            .collect()
    };
    let mut out = vec![];
    for t in chosen.into_iter().rev() {
        let obj = json!({"machine": server.opts.machine, "session": server.opts.session, "run": run.id, "turn": t.n, "native_conversation_id": t.native_conversation_id});
        out.push(src(
            "user_request",
            obj,
            format!("user request, turn {}", t.n),
            t.prompt.clone(),
            Some(t.started_at_ms),
        ));
    }
    out
}

/// Gather the selected inputs for `op`. Returns sources, valid target IDs and the scope
/// descriptor stored with the request (IDs only).
async fn gather(
    server: &Arc<Server>,
    ctx: &Ctx,
    op: Operation,
    t: &Target,
) -> Result<(Vec<SourceInput>, Vec<String>, Value), RpcError> {
    let mut sources = vec![];
    let mut targets = vec![];
    let scope = json!({
        "workspace": t.ws.id,
        "run": t.run.as_ref().map(|r| &r.id),
        "task": t.task.as_ref().map(|x| &x.id),
        "pane": t.pane.as_ref().map(|x| &x.id),
        "turns": t.turns,
        "include_screen": t.include_screen,
    });
    match op {
        Operation::SuggestTaskDetails => {
            let run = t.run.as_ref().expect("checked");
            sources = turn_sources(server, run, &t.turns, 1);
            if sources.is_empty() {
                return Err(invalid(
                    "no recorded user request matches the selection (see `vibeke task sources`)",
                ));
            }
        }
        Operation::PaneTitle => {
            let pane = t.pane.as_ref().expect("checked");
            let mut meta = format!(
                "current title: {}\ncwd: {}\nforeground: {}",
                pane.title.as_deref().unwrap_or(&pane.auto_title),
                pane.cwd.as_deref().unwrap_or("?"),
                pane.fg_cmdline.join(" ")
            );
            if let Some(run) = &t.run {
                meta.push_str(&format!(
                    "\nagent: {} ({})",
                    run.harness,
                    run.name.as_deref().unwrap_or("")
                ));
                sources.extend(turn_sources(server, run, &t.turns, 1));
                if let Some(m) = &run.last_message {
                    sources.push(src(
                        "agent_message",
                        json!({"run": run.id, "field": "last_message"}),
                        "agent's last message",
                        clip(m, 1500).0,
                        None,
                    ));
                }
            }
            sources.insert(
                0,
                src(
                    "pane",
                    json!({"pane": pane.id}),
                    "pane metadata",
                    meta,
                    None,
                ),
            );
            if t.include_screen {
                let r = read_call(
                    server,
                    ctx,
                    "pane.read",
                    json!({"pane": pane.id, "lines": 40}),
                )
                .await?;
                sources.push(src(
                    "screen",
                    json!({"pane": pane.id, "revision": r["revision"]}),
                    "screen excerpt (inferred, last 40 lines)",
                    r["text"].as_str().unwrap_or(""),
                    Some(now()),
                ));
            }
        }
        Operation::Briefing => {
            let (runs, inters, tasks) = server.with_core(|c| {
                let panes: Vec<String> = c
                    .model
                    .panes
                    .iter()
                    .filter(|p| p.workspace == t.ws.id)
                    .map(|p| p.id.clone())
                    .collect();
                let runs: Vec<AgentRun> = c
                    .model
                    .runs
                    .iter()
                    .filter(|r| panes.contains(&r.pane) && r.ended_at_ms.is_none())
                    .cloned()
                    .collect();
                let inters: Vec<Interaction> = c
                    .model
                    .interactions
                    .iter()
                    .filter(|i| panes.contains(&i.pane) && i.status == InteractionStatus::Open)
                    .cloned()
                    .collect();
                let tasks: Vec<Task> = c
                    .model
                    .tasks
                    .iter()
                    .filter(|x| x.workspace.as_deref() == Some(t.ws.id.as_str()))
                    .cloned()
                    .collect();
                (runs, inters, tasks)
            });
            for i in &inters {
                targets.push(i.id.clone());
                sources.push(src(
                    "interaction",
                    json!({"interaction": i.id, "run": i.run}),
                    format!("open interaction {} ({:?})", i.handle, i.kind),
                    format!(
                        "id: {}\nkind: {:?}\ntitle: {}\nopened_at_ms: {}\n{}",
                        i.id,
                        i.kind,
                        i.title,
                        i.opened_at_ms,
                        clip(i.body_md.as_deref().unwrap_or(""), 600).0
                    ),
                    Some(i.opened_at_ms),
                ));
            }
            for r in &runs {
                targets.push(r.id.clone());
                sources.push(src(
                    "run_state",
                    json!({"run": r.id, "pane": r.pane}),
                    format!("agent {} ({})", r.name.as_deref().unwrap_or(&r.handle), r.harness),
                    format!(
                        "id: {}\nharness: {}\nexecution: {:?}\nturns completed: {}\nlast tool: {}\nlast message (agent claim): {}",
                        r.id,
                        r.harness,
                        r.execution.value,
                        r.turns_completed,
                        r.last_tool.as_deref().unwrap_or("-"),
                        clip(r.last_message.as_deref().unwrap_or("-"), 400).0
                    ),
                    None,
                ));
            }
            for x in &tasks {
                targets.push(x.id.clone());
                sources.push(src(
                    "task",
                    json!({"task": x.id}),
                    format!("task {}", x.handle),
                    format!("id: {}\ntitle: {}\nstatus: {}", x.id, x.title, x.status),
                    Some(x.created_at_ms),
                ));
            }
            if sources.is_empty() {
                sources.push(src(
                    "workspace",
                    json!({"workspace": t.ws.id}),
                    "workspace",
                    format!(
                        "Workspace {} has no agents, open interactions or tasks.",
                        t.ws.display_name()
                    ),
                    None,
                ));
            }
        }
        Operation::ReviewSummary => {
            sources = review_source(server, ctx, t.task.as_ref().expect("checked")).await;
        }
        // 15 §8.2 (T4): the package (diff stat, checks, criteria, the deterministic heuristic)
        // is the only input; the result is a labelled estimate the user applies explicitly.
        Operation::EffortEstimate => {
            sources = review_source(server, ctx, t.task.as_ref().expect("checked")).await;
        }
        Operation::Handoff => {
            if let Some(task) = &t.task {
                sources.extend(review_source(server, ctx, task).await);
                let runs: Vec<String> = crate::tracking::task_bindings(server, &task.id)
                    .into_iter()
                    .map(|b| b.run_id)
                    .collect();
                for rid in runs.iter().rev().take(2) {
                    if let Some(r) = server.with_core(|c| c.run(rid).cloned()) {
                        sources.extend(turn_sources(server, &r, &[], 5));
                    }
                }
            }
            if let Some(run) = &t.run {
                sources.extend(turn_sources(server, run, &t.turns, 5));
                if let Some(m) = &run.last_message {
                    sources.push(src(
                        "agent_message",
                        json!({"run": run.id, "field": "last_message"}),
                        "agent's last message (claim)",
                        clip(m, 3000).0,
                        None,
                    ));
                }
            }
        }
    }
    Ok((sources, targets, scope))
}

// ---- generate / confirm -------------------------------------------------------------------------

async fn generate(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let op_name = req(p, "operation")?;
    let op = Operation::parse(op_name).ok_or_else(|| {
        invalid(format!(
            "unknown operation `{op_name}` (known: {})",
            ops::ALL
                .iter()
                .map(|o| o.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    let (cfg, patterns) = enabled_config()?;
    let resolved = cfg.resolve(s(p, "profile")).map_err(rpc)?;
    if let Some(key) = s(p, "idempotency_key")
        && let Some(prev) = all_requests(server)
            .into_iter()
            .find(|r| r.idempotency_key.as_deref() == Some(key))
    {
        if prev.operation != op.as_str() {
            return Err(err(
                ErrorKind::Conflict,
                "idempotency key used for another request",
            )
            .details(json!({"reason": "idempotency_key_reused"})));
        }
        return Ok(json!({"request": view(&prev, true), "deduplicated": true}));
    }
    // Consent is checked on metadata only, before any content is retrieved (14 §7.1).
    let target = resolve_target(server, ctx, op, p)?;
    let mut classes: Vec<&str> = op.classes().to_vec();
    if target.include_screen {
        classes.push("screen");
    }
    let ws_path = canonical(&target.ws.root_path);
    let grants = vk_assist::consent::load(&consent_path());
    let grant = vk_assist::consent::check(&grants, &ws_path, &resolved, op.as_str(), &classes)
        .map_err(rpc)?
        .clone();
    let (inputs, targets, scope) = gather(server, ctx, op, &target).await?;
    let system = op.system();
    let instructions = op.instructions(&targets);
    let redactor = vk_redact::Redactor::new(&patterns).map_err(|e| {
        ae(
            Category::NotConfigured,
            format!("[security.redact] patterns: {e}"),
        )
    })?;
    let pkg = Package::build(
        inputs,
        Limits {
            max_input_bytes: resolved.profile.max_input_bytes,
            max_input_tokens: resolved.profile.max_input_tokens,
        },
        system.len() + instructions.len(),
        &redactor,
    )
    .map_err(rpc)?;
    let payload = Payload {
        adapter: resolved.connection.adapter.as_str().into(),
        model: resolved.profile.model.clone(),
        max_output_tokens: resolved.profile.max_output_tokens,
        system,
        user: format!("{instructions}\n{}", pkg.render()),
    };
    let digest = payload.digest();
    let auto =
        cfg.auto_send_allows(op.as_str()) && grant.auto_send.iter().any(|o| o == op.as_str());
    let t = now();
    let rec = AssistRequest {
        id: format!("as_{}", crate::core::ulid().to_lowercase()),
        record: "assist".into(),
        operation: op.as_str().into(),
        state: ReqState::AwaitingConfirmation,
        workspace: target.ws.id.clone(),
        workspace_path: ws_path,
        inputs: scope,
        profile: resolved.profile_id.clone(),
        connection: resolved.connection_id.clone(),
        adapter: resolved.connection.adapter.as_str().into(),
        model: resolved.profile.model.clone(),
        endpoint_host: resolved.endpoint_host(),
        machine: server.opts.machine.clone(),
        prompt_version: ops::PROMPT_VERSION.into(),
        sources: pkg.sources.clone(),
        omitted: pkg.omitted.clone(),
        redactions: pkg.redactions,
        redaction_notice: vk_assist::context::REDACTION_NOTICE.into(),
        context_digest: pkg.context_digest(),
        preview_digest: digest.clone(),
        payload_bytes: payload.bytes(),
        estimated_input_tokens: payload.estimated_input_tokens(),
        estimation_method: vk_assist::context::ESTIMATION_METHOD.into(),
        max_output_tokens: payload.max_output_tokens,
        auto_sent: auto,
        usage: Usage::default(),
        attempts: 0,
        estimated_cost_usd: None,
        finish_reason: None,
        error: None,
        output: None,
        created_by: ctx.client_id.clone(),
        idempotency_key: s(p, "idempotency_key").map(str::to_string),
        retry_of: s(p, "retry_of").map(str::to_string),
        created_at_ms: t,
        queued_at_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
    };
    let preview = json!({
        "digest": digest,
        "system": payload.system,
        "user": payload.user,
        "model": payload.model,
        "adapter": payload.adapter,
        "endpoint_host": rec.endpoint_host,
        "execution_machine": rec.machine,
        "max_output_tokens": payload.max_output_tokens,
        "bytes": payload.bytes(),
        "estimated_input_tokens": payload.estimated_input_tokens(),
        "estimated_max_cost_usd": resolved.cost(payload.estimated_input_tokens(), payload.max_output_tokens),
        "sources": pkg.sources,
        "omitted": pkg.omitted,
        "redactions": pkg.redactions,
        "notice": vk_assist::context::REDACTION_NOTICE,
    });
    save(server, &rec, Some("assistant.request_created"))?;
    state(server).prepared.lock().unwrap().insert(
        rec.id.clone(),
        Prepared {
            payload,
            resolved,
            source_ids: pkg.source_ids(),
            targets,
            expires: Instant::now() + Duration::from_secs(cfg.preview_ttl_seconds.max(1)),
        },
    );
    if auto {
        let r = start(server, &rec.id, &cfg)?;
        return Ok(
            json!({"request": view(&r, false), "preview": preview, "requires_confirmation": false}),
        );
    }
    Ok(json!({
        "request": view(&rec, false),
        "preview": preview,
        "requires_confirmation": true,
        "confirm_with": {"method": "assistant.confirm", "params": {"request": rec.id, "preview_digest": rec.preview_digest}},
    }))
}

fn confirm(server: &Arc<Server>, p: &Value) -> R {
    let id = req(p, "request")?;
    let digest = req(p, "preview_digest")?;
    let (cfg, _) = enabled_config()?;
    let r = find(server, id)?;
    if r.state != ReqState::AwaitingConfirmation {
        return Err(err(
            ErrorKind::Conflict,
            format!("request is {}, not awaiting confirmation", r.state.as_str()),
        )
        .details(json!({"reason": "not_awaiting_confirmation", "state": r.state})));
    }
    if digest != r.preview_digest {
        return Err(err(
            ErrorKind::Conflict,
            "preview_digest does not match the previewed payload",
        )
        .details(json!({"reason": "preview_mismatch"})));
    }
    let r = start(server, &r.id, &cfg)?;
    Ok(json!({"request": view(&r, false)}))
}

/// Admission (consent recheck, queue, rate, budget reservation) and dispatch.
fn start(server: &Arc<Server>, id: &str, cfg: &AssistConfig) -> Result<AssistRequest, RpcError> {
    let st = state(server);
    let mut r = find(server, id)?;
    let (resolved, expired) = {
        let prep = st.prepared.lock().unwrap();
        match prep.get(id) {
            Some(pr) => (Some(pr.resolved.clone()), pr.expires < Instant::now()),
            None => (None, true),
        }
    };
    if expired {
        st.prepared.lock().unwrap().remove(id);
        let r = cancel_one(
            server,
            r,
            Category::Cancelled,
            "the preview expired before it was confirmed",
        )?;
        return Err(err(
            ErrorKind::Conflict,
            "the preview expired; generate a new one",
        )
        .details(json!({"reason": "preview_expired", "request": r.id})));
    }
    let resolved = resolved.expect("present");
    // The user may have revoked consent or changed the endpoint since the preview.
    let grants = vk_assist::consent::load(&consent_path());
    let op = Operation::parse(&r.operation).expect("stored op");
    let mut classes: Vec<&str> = op.classes().to_vec();
    if r.inputs["include_screen"] == true {
        classes.push("screen");
    }
    vk_assist::consent::check(&grants, &r.workspace_path, &resolved, op.as_str(), &classes)
        .map_err(rpc)?;
    if cfg.resolve(Some(&r.profile)).map(|x| x.fingerprint) != Ok(resolved.fingerprint.clone()) {
        return Err(ae(
            Category::NotConfigured,
            "the profile or connection changed since the preview; generate a new one",
        ));
    }
    let active = st.running.lock().unwrap().len()
        + all_requests(server)
            .iter()
            .filter(|x| x.state == ReqState::Queued)
            .count();
    if active >= cfg.max_concurrent_requests + cfg.max_queued_requests {
        return Err(ae(Category::QueueFull, "the assistant queue is full"));
    }
    {
        let _g = st.ledger.lock().unwrap();
        let want = Amount {
            requests: 1,
            tokens: r.estimated_input_tokens + r.max_output_tokens,
            cost_usd: resolved
                .cost(r.estimated_input_tokens, r.max_output_tokens)
                .unwrap_or(0.0),
        };
        ledger(server)
            .check(cfg, reserved_total(st), want, resolved.prices().is_some())
            .map_err(rpc)?;
        st.rate
            .lock()
            .unwrap()
            .admit(now(), cfg.requests_per_minute)
            .map_err(rpc)?;
        st.reserved.lock().unwrap().insert(id.to_string(), want);
    }
    let prepared = st.prepared.lock().unwrap().remove(id).expect("present");
    r.state = ReqState::Queued;
    r.queued_at_ms = Some(now());
    if let Err(e) = save(server, &r, None) {
        release(server, id, false, None);
        return Err(e);
    }
    let deadline = Instant::now() + Duration::from_secs(cfg.request_timeout_seconds.max(1));
    let sem = st
        .sem
        .get_or_init(|| Arc::new(Semaphore::new(cfg.max_concurrent_requests.max(1))))
        .clone();
    let mut running = st.running.lock().unwrap();
    let srv = server.clone();
    let rid = id.to_string();
    let h = tokio::spawn(async move { run(srv, rid, prepared, sem, deadline).await });
    running.insert(id.to_string(), h.abort_handle());
    Ok(r)
}

async fn run(
    server: Arc<Server>,
    id: String,
    prep: Prepared,
    sem: Arc<Semaphore>,
    deadline: Instant,
) {
    let permit = tokio::time::timeout_at(deadline.into(), sem.acquire_owned()).await;
    let Ok(Ok(_permit)) = permit else {
        finish(
            &server,
            &id,
            Err(AssistError::new(
                Category::Timeout,
                "timed out waiting in the queue",
            )),
            Usage::default(),
            0,
            None,
            &prep,
        );
        return;
    };
    let Ok(mut r) = find(&server, &id) else {
        return;
    };
    if r.state != ReqState::Queued {
        state(&server).running.lock().unwrap().remove(&id);
        return;
    }
    r.state = ReqState::Running;
    r.started_at_ms = Some(now());
    let _ = save(&server, &r, Some("assistant.request_started"));
    let key = match vk_assist::config::resolve_credential(&prep.resolved.connection) {
        Ok(k) => k,
        Err(e) => {
            finish(&server, &id, Err(e), Usage::default(), 0, None, &prep);
            return;
        }
    };
    let out =
        vk_assist::provider::generate(&prep.resolved, key.as_deref(), &prep.payload, deadline)
            .await;
    drop(key);
    let op = Operation::parse(&r.operation).expect("stored op");
    let result = out
        .result
        .and_then(|text| ops::validate(op, &text, &prep.source_ids, &prep.targets));
    finish(
        &server,
        &id,
        result,
        out.usage,
        out.attempts,
        out.finish_reason,
        &prep,
    );
}

fn finish(
    server: &Arc<Server>,
    id: &str,
    result: Result<Value, AssistError>,
    usage: Usage,
    attempts: u32,
    finish_reason: Option<String>,
    prep: &Prepared,
) {
    let st = state(server);
    st.running.lock().unwrap().remove(id);
    let Ok(mut r) = find(server, id) else { return };
    if !r.state.open() {
        return;
    }
    let cost = match (usage.input_tokens, usage.output_tokens) {
        (Some(i), Some(o)) => prep.resolved.cost(i, o),
        _ => None,
    };
    let actual = (usage.input_tokens.is_some() || usage.output_tokens.is_some()).then(|| Amount {
        requests: attempts.max(1) as u64,
        tokens: usage.total(),
        cost_usd: cost.unwrap_or(0.0),
    });
    release(server, id, attempts > 0, actual);
    r.usage = usage;
    r.attempts = attempts;
    r.estimated_cost_usd = cost;
    r.finish_reason = finish_reason;
    r.finished_at_ms = Some(now());
    match result {
        Ok(v) => {
            r.state = ReqState::Done;
            r.output = Some(v);
        }
        Err(e) => {
            r.state = ReqState::Failed;
            r.error = Some(e);
        }
    }
    let _ = save(server, &r, Some("assistant.request_finished"));
}

// ---- get / list / cancel / purge ----------------------------------------------------------------

fn get(server: &Server, p: &Value) -> R {
    let r = find(server, req(p, "request")?)?;
    Ok(json!({"request": view(&r, true)}))
}

fn list(server: &Server, p: &Value) -> R {
    let mut v = all_requests(server);
    if let Some(w) = s(p, "workspace") {
        v.retain(|r| r.workspace == w || r.workspace_path == w);
    }
    if let Some(st) = s(p, "state") {
        v.retain(|r| r.state.as_str() == st);
    }
    v.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
    v.truncate(u(p, "limit").unwrap_or(50) as usize);
    Ok(json!({"requests": v.iter().map(|r| view(r, false)).collect::<Vec<_>>()}))
}

fn cancel_one(
    server: &Server,
    mut r: AssistRequest,
    cat: Category,
    why: &str,
) -> Result<AssistRequest, RpcError> {
    let st = state(server);
    let was_running = r.state == ReqState::Running;
    if let Some(h) = st.running.lock().unwrap().remove(&r.id) {
        h.abort();
    }
    st.prepared.lock().unwrap().remove(&r.id);
    // A request cancelled mid-flight may still be billed: count its reservation.
    release(server, &r.id, was_running, None);
    r.state = ReqState::Cancelled;
    r.error = Some(AssistError::new(cat, why));
    r.finished_at_ms = Some(now());
    if was_running {
        r.attempts = r.attempts.max(1);
    }
    save(server, &r, Some("assistant.request_finished"))?;
    Ok(r)
}

fn cancel(server: &Server, p: &Value) -> R {
    let r = find(server, req(p, "request")?)?;
    if !r.state.open() {
        return Err(err(
            ErrorKind::Conflict,
            format!("request already {}", r.state.as_str()),
        )
        .details(json!({"reason": "already_finished", "state": r.state})));
    }
    let r = cancel_one(server, r, Category::Cancelled, "cancelled by the user")?;
    Ok(json!({"request": view(&r, false)}))
}

/// `assistant.purge {request | workspace | all}`: forget generated outputs and their records
/// (14 §8 `forget`). Unfinished requests in scope are cancelled first.
fn purge(server: &Server, p: &Value) -> R {
    let all = b(p, "all") == Some(true);
    let id = s(p, "request");
    let ws = s(p, "workspace");
    if !all && id.is_none() && ws.is_none() {
        return Err(invalid("purge needs --request, --workspace or --all"));
    }
    let victims: Vec<AssistRequest> = all_requests(server)
        .into_iter()
        .filter(|r| {
            all || id == Some(r.id.as_str())
                || ws.is_some_and(|w| r.workspace == w || r.workspace_path == canonical(w))
        })
        .collect();
    for r in &victims {
        if r.state.open() {
            let _ = cancel_one(server, r.clone(), Category::Cancelled, "purged");
        }
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    for r in &victims {
        tx.m.delete(K_REQ, &r.id);
    }
    tx.event(
        "assistant.purged",
        json!({}),
        json!({"count": victims.len(), "reason": "user"}),
    );
    server.commit(&mut c, tx).map_err(crate::api::internal)?;
    Ok(json!({"purged": victims.len()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_path_refuses_mutations() {
        for m in [
            "task.track",
            "task.intent.update",
            "task.message.send",
            "task.check.run",
            "task.review.accept",
            "interaction.answer",
            "pane.send_text",
            "agent.prompt",
            "assistant.generate",
            "assistant.confirm",
        ] {
            let e = check_read_only(m).unwrap_err();
            assert!(e.kind_is(ErrorKind::PermissionDenied), "{m}");
        }
        assert!(check_read_only("task.review.get").is_ok());
        for m in READ_ONLY {
            let mutating = crate::api::METHODS
                .iter()
                .find(|(n, _)| n == m)
                .map(|(_, mutating)| *mutating);
            assert_ne!(mutating, Some(true), "{m} must not be a mutation");
        }
    }

    #[test]
    fn errors_carry_category() {
        let e = rpc(AssistError::new(
            Category::PermissionDenied,
            "consent_required: no consent",
        ));
        assert!(e.kind_is(ErrorKind::PermissionDenied));
        assert_eq!(e.data.details["reason"], "consent_required");
        assert_eq!(e.data.details["category"], "permission_denied");
    }
}
