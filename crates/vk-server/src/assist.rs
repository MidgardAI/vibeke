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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use vk_assist::budget::{Amount, Ledger, RateWindow, Reservation};
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
/// Budget reservations of unfinished requests, persisted before dispatch (14 §8).
const KV_RESERVED: &str = "reservations";

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
    /// Canonical paths of further workspaces whose content was selected (each needs its own
    /// consent; such requests never auto-send).
    #[serde(default)]
    pub other_workspace_paths: Vec<String>,
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

impl AssistRequest {
    /// Every workspace whose consent this request depends on.
    fn workspace_paths(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.workspace_path).chain(&self.other_workspace_paths)
    }
}

struct Prepared {
    payload: Payload,
    resolved: Resolved,
    source_ids: Vec<String>,
    targets: Vec<String>,
    expires: Instant,
    op: Operation,
    classes: Vec<&'static str>,
    /// Canonical paths of every workspace a source came from (primary first).
    workspaces: Vec<String>,
}

#[derive(Default)]
pub struct State {
    /// Frozen payloads awaiting confirmation (bounded; expired ones are purged).
    prepared: Mutex<HashMap<String, Prepared>>,
    running: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    rate: Mutex<RateWindow>,
    /// Serializes every request state transition (confirm/start, dispatch, finish, cancel,
    /// expiry, recovery) so racing calls can't both act on one request.
    transitions: Mutex<()>,
    /// Serializes ledger + reservation updates.
    ledger: Mutex<()>,
    /// Serializes `generate` calls that carry an idempotency key.
    idem: tokio::sync::Mutex<()>,
    sem: OnceLock<Arc<Semaphore>>,
    recovered: OnceLock<()>,
}

/// Lock without propagating poison: a panic elsewhere must not disable the assistant.
fn lk<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
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

/// A config-load error without the file's content: TOML parse errors quote the offending
/// line, which may hold a credential pasted inline (14 §8). Only the location survives.
fn config_error_summary(e: &str) -> String {
    let first = e.lines().next().unwrap_or("").trim();
    let loc = first
        .find("line ")
        .map(|i| &first[i..])
        .map(|s| {
            s.chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | ','))
                .collect::<String>()
        })
        .filter(|s| !s.trim().is_empty());
    match loc {
        Some(l) => format!(
            "config.toml could not be parsed (at {})",
            l.trim_end_matches([',', ' '])
        ),
        None => "config.toml could not be loaded".into(),
    }
}

pub fn load_config() -> Result<(AssistConfig, Vec<String>), String> {
    let cfg = match vk_config::Config::load(vk_config::config_path()) {
        Ok((c, _)) => c,
        Err(e) => return Err(config_error_summary(&e.to_string())),
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
    let mut c = lk(&server.core);
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

// ---- maintenance: restart recovery, preview expiry, retention, disable ------------------------

fn maintain(server: &Arc<Server>) {
    let st = state(server);
    let _t = lk(&st.transitions);
    let first = st.recovered.set(()).is_ok();
    let cfg = load_config().map(|(c, _)| c).unwrap_or_default();
    let retention = cfg.result_retention_hours as i64 * 3_600_000;
    let t = now();
    let mut changed: Vec<AssistRequest> = vec![];
    let mut purge: Vec<String> = vec![];
    let mut dispatched: Vec<String> = vec![];
    for mut r in all_requests(server) {
        if first && r.state.open() {
            // Unfinished at startup: never replayed automatically (14 §8).
            let (st2, cat) = if r.state == ReqState::AwaitingConfirmation {
                (ReqState::Cancelled, Category::Cancelled)
            } else {
                (ReqState::Interrupted, Category::Interrupted)
            };
            if r.state == ReqState::Running {
                // It may have been sent and billed: its reservation is charged below.
                dispatched.push(r.id.clone());
                r.attempts = r.attempts.max(1);
            }
            r.state = st2;
            r.error = Some(AssistError::new(
                cat,
                "the server restarted before this request finished",
            ));
            r.finished_at_ms = Some(t);
            changed.push(r);
            continue;
        }
        if !cfg.enabled && r.state.open() {
            // 14 §10: disabling assistance cancels queued/running work (dispatch re-checks
            // too, so this also covers requests nobody polls).
            let _ = cancel_locked(server, r, Category::Disabled, "assistance was disabled");
            continue;
        }
        if r.state == ReqState::AwaitingConfirmation && preview_expired(st, &r.id) {
            let _ = cancel_locked(
                server,
                r,
                Category::Cancelled,
                "the preview expired before it was confirmed",
            );
            continue;
        }
        if !r.state.open() && r.finished_at_ms.is_some_and(|f| t - f > retention) {
            purge.push(r.id);
        }
    }
    if first {
        recover_reservations(server, &dispatched);
    }
    if changed.is_empty() && purge.is_empty() {
        return;
    }
    let mut c = lk(&server.core);
    let mut tx = Tx::new();
    for r in &changed {
        put(&mut tx, r);
        tx.event_by(
            "assistant.request_finished",
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

/// The frozen payload is gone or past its TTL.
fn preview_expired(st: &State, id: &str) -> bool {
    lk(&st.prepared)
        .get(id)
        .is_none_or(|p| p.expires <= Instant::now())
}

/// Expire one preview when its TTL passes, even if no other assistant call ever comes
/// (an abandoned preview's raw content must not outlive the TTL).
fn schedule_expiry(server: &Arc<Server>, id: &str, ttl: Duration) {
    let srv = server.clone();
    let id = id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(ttl + Duration::from_millis(50)).await;
        let st = state(&srv);
        let _t = lk(&st.transitions);
        if let Ok(r) = find(&srv, &id)
            && r.state == ReqState::AwaitingConfirmation
            && preview_expired(st, &id)
        {
            let _ = cancel_locked(
                &srv,
                r,
                Category::Cancelled,
                "the preview expired before it was confirmed",
            );
        } else if find(&srv, &id).is_err() || preview_expired(st, &id) {
            lk(&st.prepared).remove(&id);
        }
    });
}

// ---- budget ledger and durable reservations ---------------------------------------------------

fn ledger(server: &Server) -> Ledger {
    let mut l: Ledger = server
        .with_core(|c| c.store.kv_get(KV_SCOPE, KV_LEDGER).ok().flatten())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    l.roll(now());
    l
}

fn reservations(server: &Server) -> HashMap<String, Reservation> {
    server
        .with_core(|c| c.store.kv_get(KV_SCOPE, KV_RESERVED).ok().flatten())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the ledger (when given) and the reservations in one commit.
fn store_budget(
    server: &Server,
    l: Option<&Ledger>,
    res: &HashMap<String, Reservation>,
) -> Result<(), RpcError> {
    let mut c = lk(&server.core);
    let mut tx = Tx::new();
    if let Some(l) = l {
        tx.m.kv(
            KV_SCOPE,
            KV_LEDGER,
            Some(serde_json::to_string(l).unwrap_or_default()),
        );
    }
    tx.m.kv(
        KV_SCOPE,
        KV_RESERVED,
        Some(serde_json::to_string(res).unwrap_or_default()),
    );
    server.commit(&mut c, tx).map_err(crate::api::internal)?;
    Ok(())
}

/// Today's reserved allowance across this coordinator's unfinished requests.
fn reserved_total(server: &Server) -> Amount {
    let day = vk_assist::budget::utc_day(now());
    let mut t = Amount::default();
    for r in reservations(server).values().filter(|r| r.day == day) {
        t.add(r.amount());
    }
    t
}

/// Admit one provider attempt for `id`: budget check against used + reserved, the rate
/// window, then the reservation is extended and **persisted before** the attempt is sent.
fn admit_attempt(
    server: &Server,
    cfg: &AssistConfig,
    id: &str,
    resolved: &Resolved,
    input_tokens: u64,
    output_tokens: u64,
) -> Result<(), AssistError> {
    let st = state(server);
    let _g = lk(&st.ledger);
    let cost = resolved.cost(input_tokens, output_tokens).unwrap_or(0.0);
    let want = Reservation::attempt(input_tokens, output_tokens, cost);
    ledger(server).check(
        cfg,
        reserved_total(server),
        want,
        resolved.prices().is_some(),
    )?;
    lk(&st.rate).admit(now(), cfg.requests_per_minute)?;
    let mut all = reservations(server);
    let day = vk_assist::budget::utc_day(now());
    let r = all.entry(id.to_string()).or_insert(Reservation {
        day,
        ..Default::default()
    });
    r.add_attempt(input_tokens, output_tokens, cost);
    store_budget(server, None, &all).map_err(|_| {
        AssistError::new(
            Category::ProviderUnavailable,
            "could not record the budget reservation",
        )
    })
}

fn mark_dispatched(server: &Server, id: &str) {
    let st = state(server);
    let _g = lk(&st.ledger);
    let mut all = reservations(server);
    if let Some(r) = all.get_mut(id) {
        r.dispatched = true;
        let _ = store_budget(server, None, &all);
    }
}

/// How a finished request's reservation is charged.
enum Charge {
    /// Never sent: the reservation is dropped.
    Nothing,
    /// Possibly sent, usage unknown: the whole reservation.
    Reserved,
    /// Reported usage over `attempts`; unknown components keep their reservation.
    Settle {
        attempts: u32,
        usage: Usage,
        prices: Option<(f64, f64)>,
    },
}

fn release(server: &Server, id: &str, charge: Charge) {
    let st = state(server);
    let _g = lk(&st.ledger);
    let mut all = reservations(server);
    let Some(res) = all.remove(id) else {
        return;
    };
    let amount = match charge {
        Charge::Nothing => None,
        Charge::Reserved => Some(res.amount()),
        Charge::Settle { attempts: 0, .. } => None,
        Charge::Settle {
            attempts,
            usage,
            prices,
        } => Some(res.settle(attempts, usage.input_tokens, usage.output_tokens, prices)),
    };
    let mut l = ledger(server);
    let charged = amount.filter(|_| res.day == l.day).map(|a| l.used.add(a));
    let _ = store_budget(server, charged.map(|_| &l), &all);
}

/// After a restart: a reservation whose request was dispatched (`running`) is charged in full
/// — the provider may have billed it — and every other leftover reservation is dropped.
fn recover_reservations(server: &Server, running: &[String]) {
    let st = state(server);
    let _g = lk(&st.ledger);
    let all = reservations(server);
    if all.is_empty() {
        return;
    }
    let mut l = ledger(server);
    for (id, r) in &all {
        if (r.dispatched || running.contains(id)) && r.day == l.day {
            l.used.add(r.amount());
        }
    }
    let _ = store_budget(server, Some(&l), &HashMap::new());
}

// ---- status / providers / consent ---------------------------------------------------------------

fn status(server: &Server) -> R {
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
        "today": {"utc_day": l.day, "used": l.used, "reserved": reserved_total(server), "remaining": l.remaining(&cfg, reserved_total(server))},
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
    let mut c = lk(&server.core);
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
    // Revocation cancels this session's unfinished requests that depend on this workspace.
    // Other sessions sharing the consent file re-read it before every dispatch and retry.
    let mut cancelled = 0;
    for r in all_requests(server) {
        if r.state.open()
            && r.workspace_paths().any(|w| *w == path)
            && s(p, "connection").is_none_or(|c| c == r.connection)
            && cancel_one(server, r, Category::PermissionDenied, "consent revoked").is_ok()
        {
            cancelled += 1;
        }
    }
    let mut c = lk(&server.core);
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
    /// Further workspaces a selected object belongs to, with what was selected there.
    others: Vec<(Workspace, String)>,
    /// Handoff: the task's bound runs whose workspace is known (each is consent-checked).
    bound_runs: Vec<AgentRun>,
    /// Handoff: bound runs left out because their workspace can't be determined.
    excluded_runs: Vec<String>,
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

/// The workspace a run belongs to (through its pane); `None` when that can't be determined.
fn ws_id_of_run(server: &Server, r: &AgentRun) -> Option<String> {
    server.with_core(|c| c.pane(&r.pane).map(|p| p.workspace.clone()))
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
    // Every selected object's own workspace (14 §6: consent is per workspace). A selection
    // whose workspace can't be determined is refused rather than attributed to another one.
    let unknown = |what: String| {
        rpc(AssistError::new(
            Category::PermissionDenied,
            format!(
                "workspace_unknown: the workspace of {what} cannot be determined, so its consent can't be checked"
            ),
        ))
    };
    let mut members: Vec<(String, String)> = vec![]; // (workspace id, what)
    if let Some(pn) = &pane {
        members.push((pn.workspace.clone(), format!("pane {}", pn.id)));
    }
    if let Some(r) = &run {
        let w = ws_id_of_run(server, r).ok_or_else(|| unknown(format!("run {}", r.id)))?;
        members.push((w, format!("run {}", r.id)));
    }
    if let Some(t) = &task {
        let w = t
            .workspace
            .clone()
            .or_else(|| {
                crate::tracking::task_bindings(server, &t.id)
                    .last()
                    .and_then(|b| server.with_core(|c| c.run(&b.run_id).cloned()))
                    .and_then(|r| ws_id_of_run(server, &r))
            })
            .ok_or_else(|| unknown(format!("task {}", t.handle)))?;
        members.push((w, format!("task {}", t.handle)));
    }
    // Handoff also reads the task's bound runs: each one is a source with its own workspace.
    let mut bound_runs = vec![];
    let mut excluded_runs = vec![];
    if op == Operation::Handoff
        && let Some(t) = &task
    {
        let ids: Vec<String> = crate::tracking::task_bindings(server, &t.id)
            .into_iter()
            .map(|b| b.run_id)
            .collect();
        for rid in ids.iter().rev().take(2) {
            let Some(r) = server.with_core(|c| c.run(rid).cloned()) else {
                continue;
            };
            match ws_id_of_run(server, &r) {
                Some(w) => {
                    members.push((w, format!("run {} (bound to task {})", r.id, t.handle)));
                    bound_runs.push(r);
                }
                // Unverifiable workspace: not sent (listed in the request's inputs).
                None => excluded_runs.push(r.id.clone()),
            }
        }
    }
    let explicit = input_s(p, "workspace").or_else(|| {
        p.get("scope")
            .and_then(|s| s.get("workspace"))
            .and_then(Value::as_str)
    });
    let ws = match (explicit, members.first()) {
        (Some(w), _) => crate::api::resolve_ws(server, ctx, Some(w))?,
        (None, Some((w, _))) => crate::api::resolve_ws(server, ctx, Some(w))?,
        (None, None) => crate::api::resolve_ws(server, ctx, None)?,
    };
    let mut others: Vec<(Workspace, String)> = vec![];
    for (w, what) in members {
        if w == ws.id || others.iter().any(|(o, _)| o.id == w) {
            continue;
        }
        let other = server
            .with_core(|c| c.ws(&w).cloned())
            .ok_or_else(|| unknown(what.clone()))?;
        others.push((other, what));
    }
    let need = |what: &str| invalid(format!("{} needs {what}", op.as_str()));
    match op {
        Operation::SuggestTaskDetails if run.is_none() => {
            return Err(need("--run or --pane (the selected request)"));
        }
        Operation::PaneTitle if pane.is_none() => return Err(need("--pane")),
        Operation::ReviewSummary if task.is_none() => return Err(need("--task")),
        Operation::Handoff if task.is_none() && run.is_none() => {
            return Err(need("--task or --run"));
        }
        _ => {}
    }
    Ok(Target {
        ws,
        others,
        bound_runs,
        excluded_runs,
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
        "other_workspaces": t.others.iter().map(|(w, _)| &w.id).collect::<Vec<_>>(),
        "bound_runs": t.bound_runs.iter().map(|r| &r.id).collect::<Vec<_>>(),
        "excluded_runs": t.excluded_runs,
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
        Operation::Handoff => {
            if let Some(task) = &t.task {
                sources.extend(review_source(server, ctx, task).await);
                // Only bound runs whose workspace was resolved (and consent-checked).
                for r in &t.bound_runs {
                    sources.extend(turn_sources(server, r, &[], 5));
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
    let st = state(server);
    // Concurrent calls carrying one idempotency key create one request: keyed calls are
    // serialized, so the second sees the first's record.
    let _idem = match s(p, "idempotency_key") {
        Some(_) => Some(st.idem.lock().await),
        None => None,
    };
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
    // A selected object from another workspace needs that workspace's own consent.
    let mut other_paths: Vec<String> = vec![];
    for (w, what) in &target.others {
        let path = canonical(&w.root_path);
        vk_assist::consent::check(&grants, &path, &resolved, op.as_str(), &classes).map_err(
            |e| {
                rpc(AssistError::new(
                    e.category,
                    format!(
                        "{} ({what} belongs to workspace {}, not {}; each workspace needs its own consent)",
                        e.message,
                        w.display_name(),
                        target.ws.display_name()
                    ),
                ))
            },
        )?;
        other_paths.push(path);
    }
    // Unconfirmed previews hold assembled content in memory: bounded.
    {
        let cap = (cfg.max_queued_requests + cfg.max_concurrent_requests).max(1);
        let live = lk(&st.prepared)
            .values()
            .filter(|x| x.expires > Instant::now())
            .count();
        if live >= cap {
            return Err(ae(
                Category::QueueFull,
                format!(
                    "too many unconfirmed previews ({live}); confirm or cancel one before generating another"
                ),
            ));
        }
    }
    let (inputs, targets, scope) = gather(server, ctx, op, &target).await?;
    let system = op.system();
    let instructions = op.instructions(&targets);
    let redactor = vk_redact::Redactor::new(&patterns).map_err(|_| {
        ae(
            Category::NotConfigured,
            "[security.redact] patterns: a pattern is not a valid regular expression",
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
    // Auto-send needs the operation on both the config's and the consent's lists, and a
    // single-workspace selection (a cross-workspace selection always previews).
    let auto = cfg.auto_send_allows(op.as_str())
        && grant.auto_send.iter().any(|o| o == op.as_str())
        && other_paths.is_empty();
    let t = now();
    let rec = AssistRequest {
        id: format!("as_{}", crate::core::ulid().to_lowercase()),
        record: "assist".into(),
        operation: op.as_str().into(),
        state: ReqState::AwaitingConfirmation,
        workspace: target.ws.id.clone(),
        workspace_path: ws_path.clone(),
        other_workspace_paths: other_paths.clone(),
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
    let ttl = Duration::from_secs(cfg.preview_ttl_seconds.max(1));
    let workspaces: Vec<String> = std::iter::once(ws_path).chain(other_paths).collect();
    {
        // The frozen payload exists before the record does, so maintenance never sees an
        // awaiting request without its payload.
        let _t = lk(&st.transitions);
        lk(&st.prepared).insert(
            rec.id.clone(),
            Prepared {
                payload,
                resolved,
                source_ids: pkg.source_ids(),
                targets,
                expires: Instant::now() + ttl,
                op,
                classes,
                workspaces,
            },
        );
        if let Err(e) = save(server, &rec, Some("assistant.request_created")) {
            lk(&st.prepared).remove(&rec.id);
            return Err(e);
        }
    }
    schedule_expiry(server, &rec.id, ttl);
    if auto {
        let r = {
            let _t = lk(&st.transitions);
            start_locked(server, rec.clone(), &cfg)?
        };
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

fn not_awaiting(r: &AssistRequest) -> RpcError {
    err(
        ErrorKind::Conflict,
        format!("request is {}, not awaiting confirmation", r.state.as_str()),
    )
    .details(json!({"reason": "not_awaiting_confirmation", "state": r.state}))
}

fn confirm(server: &Arc<Server>, p: &Value) -> R {
    let id = req(p, "request")?;
    let digest = req(p, "preview_digest")?;
    let (cfg, _) = enabled_config()?;
    let st = state(server);
    // State check, payload hand-off and admission happen under one lock: of two racing
    // confirmations (or a confirmation racing a cancel or an expiry) exactly one proceeds.
    let _t = lk(&st.transitions);
    let r = find(server, id)?;
    if r.state != ReqState::AwaitingConfirmation {
        return Err(not_awaiting(&r));
    }
    if digest != r.preview_digest {
        return Err(err(
            ErrorKind::Conflict,
            "preview_digest does not match the previewed payload",
        )
        .details(json!({"reason": "preview_mismatch"})));
    }
    let r = start_locked(server, r, &cfg)?;
    Ok(json!({"request": view(&r, false)}))
}

/// Everything that must still hold right before content leaves: assistance enabled, the same
/// connection (adapter, endpoint, credential reference) as previewed, and a current consent
/// grant for every workspace a source came from. Re-read from disk each time, so a disable
/// or a revocation from any session takes effect at the next dispatch or retry.
fn dispatch_check(
    cfg: &AssistConfig,
    r: &AssistRequest,
    prep: &Prepared,
) -> Result<(), AssistError> {
    if !cfg.enabled {
        return Err(AssistError::new(
            Category::Disabled,
            "assistance was disabled",
        ));
    }
    match cfg.resolve(Some(&r.profile)) {
        Ok(x)
            if x.fingerprint == prep.resolved.fingerprint
                && x.connection == prep.resolved.connection => {}
        _ => {
            return Err(AssistError::new(
                Category::NotConfigured,
                "the profile or connection changed since the preview; generate a new one",
            ));
        }
    }
    let grants = vk_assist::consent::load(&consent_path());
    for w in &prep.workspaces {
        vk_assist::consent::check(&grants, w, &prep.resolved, prep.op.as_str(), &prep.classes)?;
    }
    Ok(())
}

/// Admission (dispatch checks, queue, budget reservation, rate window) and dispatch. The
/// caller holds `transitions`.
fn start_locked(
    server: &Arc<Server>,
    mut r: AssistRequest,
    cfg: &AssistConfig,
) -> Result<AssistRequest, RpcError> {
    let st = state(server);
    let id = r.id.clone();
    let prep = lk(&st.prepared)
        .remove(&id)
        .filter(|p| p.expires > Instant::now());
    let Some(prep) = prep else {
        let r = cancel_locked(
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
    };
    // A refused admission keeps the preview confirmable (e.g. after raising a limit).
    let back = |prep: Prepared| {
        lk(&st.prepared).insert(id.clone(), prep);
    };
    if let Err(e) = dispatch_check(cfg, &r, &prep) {
        back(prep);
        return Err(rpc(e));
    }
    let active = lk(&st.running).len()
        + all_requests(server)
            .iter()
            .filter(|x| x.state == ReqState::Queued)
            .count();
    if active >= cfg.max_concurrent_requests + cfg.max_queued_requests {
        back(prep);
        return Err(ae(Category::QueueFull, "the assistant queue is full"));
    }
    if let Err(e) = admit_attempt(
        server,
        cfg,
        &id,
        &prep.resolved,
        r.estimated_input_tokens,
        r.max_output_tokens,
    ) {
        back(prep);
        return Err(rpc(e));
    }
    r.state = ReqState::Queued;
    r.queued_at_ms = Some(now());
    if let Err(e) = save(server, &r, None) {
        release(server, &id, Charge::Nothing);
        back(prep);
        return Err(e);
    }
    let deadline = Instant::now() + Duration::from_secs(cfg.request_timeout_seconds.max(1));
    let sem = st
        .sem
        .get_or_init(|| Arc::new(Semaphore::new(cfg.max_concurrent_requests.max(1))))
        .clone();
    let mut running = lk(&st.running);
    let srv = server.clone();
    let rid = id.clone();
    let h = tokio::spawn(async move { run(srv, rid, prep, sem, deadline).await });
    running.insert(id, h.abort_handle());
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
    let st = state(&server);
    {
        let _t = lk(&st.transitions);
        let Ok(mut r) = find(&server, &id) else {
            lk(&st.running).remove(&id);
            release(&server, &id, Charge::Nothing);
            return;
        };
        if r.state != ReqState::Queued {
            lk(&st.running).remove(&id);
            return;
        }
        // Re-read enabled state, consent and endpoint right before sending: a request that
        // waited in the queue must not outlive a disable or a revocation (from any session).
        let check = load_config()
            .map_err(|_| {
                AssistError::new(
                    Category::NotConfigured,
                    "the configuration can no longer be loaded",
                )
            })
            .and_then(|(cfg, _)| dispatch_check(&cfg, &r, &prep));
        if let Err(e) = check {
            let _ = cancel_locked(&server, r, e.category, &e.message);
            return;
        }
        r.state = ReqState::Running;
        r.started_at_ms = Some(now());
        let _ = save(&server, &r, Some("assistant.request_started"));
        mark_dispatched(&server, &id);
    }
    let key = match vk_assist::config::resolve_credential(&prep.resolved.connection) {
        Ok(k) => k,
        Err(e) => {
            finish(&server, &id, Err(e), Usage::default(), 0, None, &prep);
            return;
        }
    };
    let out = vk_assist::provider::generate_gated(
        &prep.resolved,
        key.as_deref(),
        &prep.payload,
        deadline,
        |n| {
            if n == 1 {
                // Admitted and checked above.
                Ok(())
            } else {
                retry_gate(&server, &id, &prep)
            }
        },
    )
    .await;
    drop(key);
    let result = out
        .result
        .and_then(|text| ops::validate(prep.op, &text, &prep.source_ids, &prep.targets));
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

/// An automatic retry is a new provider attempt: it passes the same dispatch checks and is
/// admitted (reserved, rate-counted) like the first one.
fn retry_gate(server: &Arc<Server>, id: &str, prep: &Prepared) -> Result<(), AssistError> {
    let st = state(server);
    let _t = lk(&st.transitions);
    let r = find(server, id)
        .map_err(|_| AssistError::new(Category::Cancelled, "the request no longer exists"))?;
    if r.state != ReqState::Running {
        return Err(AssistError::new(
            Category::Cancelled,
            "the request was cancelled",
        ));
    }
    let (cfg, _) = load_config().map_err(|_| {
        AssistError::new(
            Category::NotConfigured,
            "the configuration can no longer be loaded",
        )
    })?;
    dispatch_check(&cfg, &r, prep)?;
    admit_attempt(
        server,
        &cfg,
        id,
        &prep.resolved,
        r.estimated_input_tokens,
        r.max_output_tokens,
    )
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
    let _t = lk(&st.transitions);
    lk(&st.running).remove(id);
    let charge = Charge::Settle {
        attempts,
        usage,
        prices: prep.resolved.prices(),
    };
    let Ok(mut r) = find(server, id) else {
        // Purged meanwhile: the attempts still count.
        release(server, id, charge);
        return;
    };
    if !r.state.open() {
        // Cancelled meanwhile: the cancellation already charged the reservation.
        return;
    }
    let cost = match (usage.input_tokens, usage.output_tokens) {
        (Some(i), Some(o)) => prep.resolved.cost(i, o),
        _ => None,
    };
    release(server, id, charge);
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

fn already_finished(r: &AssistRequest) -> RpcError {
    err(
        ErrorKind::Conflict,
        format!("request already {}", r.state.as_str()),
    )
    .details(json!({"reason": "already_finished", "state": r.state}))
}

/// Cancel an unfinished request (re-read under the transitions lock).
fn cancel_one(
    server: &Server,
    r: AssistRequest,
    cat: Category,
    why: &str,
) -> Result<AssistRequest, RpcError> {
    let st = state(server);
    let _t = lk(&st.transitions);
    let fresh = find(server, &r.id)?;
    if !fresh.state.open() {
        return Err(already_finished(&fresh));
    }
    cancel_locked(server, fresh, cat, why)
}

/// The caller holds `transitions`.
fn cancel_locked(
    server: &Server,
    mut r: AssistRequest,
    cat: Category,
    why: &str,
) -> Result<AssistRequest, RpcError> {
    let st = state(server);
    let was_running = r.state == ReqState::Running;
    if let Some(h) = lk(&st.running).remove(&r.id) {
        h.abort();
    }
    lk(&st.prepared).remove(&r.id);
    // A request cancelled mid-flight may still be billed: charge its whole reservation.
    release(
        server,
        &r.id,
        if was_running {
            Charge::Reserved
        } else {
            Charge::Nothing
        },
    );
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
        return Err(already_finished(&r));
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
    let mut c = lk(&server.core);
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
