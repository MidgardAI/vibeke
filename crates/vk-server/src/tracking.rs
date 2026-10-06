//! Optional task tracking (spec 15 T1): turn/item capture from adapter signals, attached tasks,
//! confirmed intent revisions, verified run bindings, idempotent mutations with receipts, and
//! clarification messages that are sent only when it is provably safe.
//!
//! Nothing here runs on the render path or writes PTY bytes, except `task.message.send` after an
//! explicit user action and the safety checks in §9.

use crate::Server;
use crate::api::{Ctx, R, err, internal, invalid, not_found, req, s, u};
use crate::core::Tx;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vk_proto::model::*;
use vk_proto::rpc::ErrorKind;
use vk_review::binding::{
    self, BindRequest, BindingRole, BindingState, IdentityEvidence, TaskRunBinding,
};
use vk_review::intent::{
    self, CommunicationRecord, CommunicationVia, DraftConstraint, DraftCriterion, Evaluation,
    IntentDraft, StopAt, TaskIntent,
};
use vk_review::{Actor, ActorKind, SourceRef};

pub(crate) const K_TURN: &str = "turn";
pub(crate) const K_ITEM: &str = "tool_item";
pub(crate) const K_INTENT: &str = "task_intent";
pub const K_BINDING: &str = "task_binding";
const K_RECEIPT: &str = "op_receipt";
pub const K_MESSAGE: &str = "task_message";
const K_BASELINE: &str = "task_baseline";
const K_COMM: &str = "task_comm";

/// Bounded copy of a user prompt kept with turns and intent excerpts (15 §4.1).
pub const MAX_PROMPT_BYTES: usize = 8192;
/// How long mutation receipts are kept for `task.operation.get` (15 §10.3).
pub const RECEIPT_WINDOW_MS: i64 = 30 * 24 * 3600 * 1000;

#[derive(Default)]
pub struct State {
    /// Messages whose delivery attempt is running in this server process.
    inflight: Mutex<HashSet<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TurnRecord {
    pub id: String,
    pub run: String,
    pub n: u32,
    pub native_conversation_id: Option<String>,
    /// Exact user prompt text (bounded). Several prompts submitted within one turn are joined.
    pub prompt: String,
    pub prompt_truncated: bool,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub last_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub key: String,
    pub method: String,
    pub payload_digest: String,
    pub result: Value,
    pub at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageState {
    Prepared,
    Sending,
    Delivered,
    DeliveryUnknown,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMessage {
    pub id: String,
    pub task: String,
    pub binding: String,
    pub run: String,
    pub native_conversation_id: String,
    pub intent_revision: Option<u32>,
    pub text: String,
    pub state: MessageState,
    pub detail: Option<String>,
    /// Criteria versions this message communicates when delivered (15 §2.3).
    pub covers: Vec<(String, u32)>,
    pub idempotency_key: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

fn now() -> i64 {
    vk_store::now_ms()
}

pub fn user(ctx: &Ctx) -> Actor {
    Actor {
        kind: ActorKind::User,
        id: if ctx.client_id.is_empty() {
            "local".into()
        } else {
            ctx.client_id.clone()
        },
    }
}

pub fn conflict(reason: &str, msg: impl Into<String>) -> vk_proto::rpc::RpcError {
    err(ErrorKind::Conflict, msg).details(json!({"reason": reason}))
}

fn truncate(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

// ---- observation: turns and tool items ----------------------------------------------------------

/// Record turn/item facts from a structured adapter signal. Called with the run as it was
/// *before* the signal was applied.
pub fn observe(server: &Arc<Server>, run: &AgentRun, event: &str, p: &Value) {
    let n = run.turns_completed + 1;
    let turn_id = format!("{}:{n}", run.id);
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    // Bindings closed at this boundary and whether a turn settled: spec 15 T2 review hooks run
    // after the commit, off the state path.
    let mut closed: Vec<TaskRunBinding> = vec![];
    let mut settled = false;
    match event {
        "SessionStart" => {
            let new_sid = p.get("session_id").and_then(Value::as_str);
            if let (Some(old), Some(new)) = (run.harness_session_id.as_deref(), new_sid)
                && old != new
            {
                // Suspension pins the end candidate like a close (15 §4.2).
                closed.extend(conversation_changed(&c, &mut tx, run, new, n));
            }
            if let Some(sid) = new_sid {
                resume_binding(&c, &mut tx, run, sid, n, &server.opts.machine);
            }
        }
        "UserPromptSubmit" => {
            let Some(text) = p.get("prompt").and_then(Value::as_str) else {
                return;
            };
            let mut t = c
                .store
                .find::<TurnRecord>(K_TURN, &turn_id)
                .ok()
                .flatten()
                .filter(|t| t.ended_at_ms.is_none())
                .unwrap_or(TurnRecord {
                    id: turn_id.clone(),
                    run: run.id.clone(),
                    n,
                    native_conversation_id: p
                        .get("session_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| run.harness_session_id.clone()),
                    prompt: String::new(),
                    prompt_truncated: false,
                    started_at_ms: now(),
                    ended_at_ms: None,
                    last_message: None,
                });
            let joined = if t.prompt.is_empty() {
                text.to_string()
            } else {
                format!("{}\n\n{text}", t.prompt)
            };
            let (prompt, cut) = truncate(&joined, MAX_PROMPT_BYTES);
            t.prompt = prompt;
            t.prompt_truncated |= cut
                || p.get("prompt_truncated")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
            tx.m.put(K_TURN, &t.id, None, &t);
            // A queued binding switch takes effect at this boundary.
            closed.extend(apply_pending_switches(&c, &mut tx, run, n));
        }
        "Stop" | "StopFailure" => {
            if let Some(mut t) = c.store.find::<TurnRecord>(K_TURN, &turn_id).ok().flatten() {
                t.ended_at_ms = Some(now());
                t.last_message = p
                    .get("last_assistant_message")
                    .and_then(Value::as_str)
                    .map(|m| truncate(m, 2000).0);
                tx.m.close(K_TURN, &t.id, None, &t);
                settled = true;
            }
        }
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
            let tool = p
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let call = p
                .get("tool_use_id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{}-{}", tool, now()));
            let id = format!("{}:{call}", run.id);
            let input = p.get("tool_input").cloned().unwrap_or(Value::Null);
            let mut rec = c
                .store
                .find::<vk_review::checks::ToolRecord>(K_ITEM, &id)
                .ok()
                .flatten()
                .unwrap_or(vk_review::checks::ToolRecord {
                    run_id: run.id.clone(),
                    turn: Some(n),
                    item_id: call.clone(),
                    tool: tool.clone(),
                    command: shell_command(&tool, &input),
                    cwd: input
                        .get("cwd")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| run.cwd.clone()),
                    exit_code: None,
                    started_at_ms: Some(now()),
                    ended_at_ms: None,
                    source: vk_review::checks::ToolRecordSource::NativeTool,
                    text: None,
                    established_subject: None,
                });
            if rec.command.is_none() {
                rec.command = shell_command(&tool, &input);
            }
            if event == "PreToolUse" {
                tx.m.put(K_ITEM, &id, None, &rec);
            } else {
                rec.ended_at_ms = Some(now());
                rec.exit_code =
                    exit_code(p).or(if event == "PostToolUseFailure" && rec.command.is_some() {
                        None
                    } else {
                        rec.exit_code
                    });
                tx.m.close(K_ITEM, &id, None, &rec);
            }
        }
        _ => return,
    }
    if !tx.m.is_empty() {
        let _ = server.commit(&mut c, tx);
    }
    drop(c);
    if !closed.is_empty() {
        crate::review::on_bindings_closed(server, closed);
    }
    if settled {
        crate::review::on_turn_settled(server, &run.id);
    }
    crate::review::interval::on_tool_signal(server, run, event, p);
    if matches!(event, "SessionStart" | "UserPromptSubmit") {
        sync_run_tasks(server);
    }
}

/// `AgentRun.task` is a projection of the run's active implementation binding (15 §4.3).
pub fn sync_run_tasks(server: &Server) {
    let mut c = server.core.lock().unwrap();
    let active: HashMap<String, String> = c
        .store
        .load::<TaskRunBinding>(K_BINDING)
        .unwrap_or_default()
        .into_iter()
        .filter(|b| b.state == BindingState::Active && b.role == BindingRole::Implementation)
        .map(|b| (b.run_id, b.task_id))
        .collect();
    let mut tx = Tx::new();
    for r in c.model.runs.iter().filter(|r| r.ended_at_ms.is_none()) {
        let want = active.get(&r.id).cloned();
        if r.task != want {
            let mut r2 = r.clone();
            r2.task = want;
            tx.run(r2);
        }
    }
    if !tx.m.is_empty() {
        let _ = server.commit(&mut c, tx);
    }
}

pub(crate) fn shell_command(tool: &str, input: &Value) -> Option<String> {
    let shellish = matches!(
        tool,
        "Bash" | "bash" | "shell" | "Shell" | "exec_command" | "local_shell" | "unified_exec"
    );
    if !shellish {
        return None;
    }
    match input.get("command").or_else(|| input.get("cmd")) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(a)) => Some(
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

fn exit_code(p: &Value) -> Option<i32> {
    let r = p.get("tool_response").unwrap_or(&Value::Null);
    [
        p.get("exit_code"),
        r.get("exit_code"),
        r.get("exitCode"),
        r.get("returnCode"),
        r.get("code"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_i64)
    .map(|c| c as i32)
}

pub fn turns_of(server: &Server, run: &str, limit: usize) -> Vec<TurnRecord> {
    server.with_core(|c| {
        let mut v: Vec<TurnRecord> = c
            .store
            .load_by_field(K_TURN, "$.run", run)
            .unwrap_or_default();
        v.sort_by_key(|t| std::cmp::Reverse(t.n));
        v.dedup_by_key(|t| t.n);
        v.truncate(limit);
        v
    })
}

pub fn items_of(server: &Server, run: &str) -> Vec<vk_review::checks::ToolRecord> {
    server.with_core(|c| {
        let mut v: Vec<vk_review::checks::ToolRecord> = c
            .store
            .load_by_field(K_ITEM, "$.run_id", run)
            .unwrap_or_default();
        v.sort_by_key(|r| r.started_at_ms);
        v
    })
}

// ---- bindings -----------------------------------------------------------------------------------

/// Bindings of one run / one task / one native conversation (indexed JSON lookups, open and
/// closed alike) — never a global "latest N" scan (Codex G02 #13).
fn run_bindings(c: &crate::core::Core, run: &str) -> Vec<TaskRunBinding> {
    sorted(
        c.store
            .load_by_field(K_BINDING, "$.run_id", run)
            .unwrap_or_default(),
    )
}

fn conv_bindings(c: &crate::core::Core, conv: &str) -> Vec<TaskRunBinding> {
    sorted(
        c.store
            .load_by_field(K_BINDING, "$.native_conversation_id", conv)
            .unwrap_or_default(),
    )
}

fn sorted(mut v: Vec<TaskRunBinding>) -> Vec<TaskRunBinding> {
    v.sort_by_key(|b| b.created_at_ms);
    v
}

pub fn bindings(c: &crate::core::Core) -> Vec<TaskRunBinding> {
    let mut v: Vec<TaskRunBinding> = c.store.load(K_BINDING).unwrap_or_default();
    v.extend(
        c.store
            .load_closed::<TaskRunBinding>(K_BINDING, 5000)
            .unwrap_or_default(),
    );
    v.sort_by_key(|b| b.created_at_ms);
    v
}

fn put_binding(tx: &mut Tx, b: &TaskRunBinding) {
    if b.state == BindingState::Closed {
        tx.m.close(K_BINDING, &b.id, None, b);
    } else {
        tx.m.put(K_BINDING, &b.id, None, b);
    }
}

/// `/clear`, `/new`, a fork or a resume to another conversation suspends automatic association
/// (15 §4.2); the user chooses Continue task or Track new work.
fn conversation_changed(
    c: &crate::core::Core,
    tx: &mut Tx,
    run: &AgentRun,
    new_sid: &str,
    n: u32,
) -> Vec<TaskRunBinding> {
    let mut suspended = vec![];
    for b in run_bindings(c, &run.id)
        .into_iter()
        .filter(|b| b.state == BindingState::Active)
    {
        if let binding::BindingDecision::Suspend { .. } =
            binding::on_conversation_change(&b, Some(new_sid))
        {
            let sb = binding::suspend(&b, n);
            put_binding(tx, &sb);
            tx.event(
                "task.binding_changed",
                json!({"task": b.task_id, "run": run.id, "binding": b.id}),
                json!({"state": "suspended", "reason": "conversation_changed", "offers": ["continue_task", "track_new_work"]}),
            );
            suspended.push(sb);
        }
    }
    suspended
}

/// Verified same-session resume (15 §4.2): a new run reporting the native session of an ended
/// run with an open binding continues that binding (continuation edge, history untouched).
/// Several candidates → no automatic choice (`task.binding_changed` with `ambiguous`).
fn resume_binding(
    c: &crate::core::Core,
    tx: &mut Tx,
    run: &AgentRun,
    sid: &str,
    n: u32,
    machine: &str,
) {
    if run_bindings(c, &run.id)
        .iter()
        .any(|b| b.state != BindingState::Closed)
    {
        return;
    }
    let facts = |r: &AgentRun, sid: Option<&str>, active: bool| binding::RunFacts {
        run_id: r.id.clone(),
        harness: r.harness.clone(),
        native_session_id: sid.map(str::to_string),
        repo_root: r.cwd.clone().unwrap_or_default(),
        owner: machine.to_string(),
        active,
        identity_verified: true,
    };
    let preds: Vec<(TaskRunBinding, binding::RunFacts)> = conv_bindings(c, sid)
        .into_iter()
        .filter(|b| {
            b.state != BindingState::Closed && b.native_conversation_id == sid && b.run_id != run.id
        })
        .filter_map(|b| {
            let live = c.run(&b.run_id).cloned();
            let old = live
                .clone()
                .or_else(|| c.store.find::<AgentRun>("run", &b.run_id).ok().flatten())?;
            let f = facts(
                &old,
                old.harness_session_id.as_deref(),
                live.is_some_and(|r| r.ended_at_ms.is_none()),
            );
            Some((b, f))
        })
        .collect();
    if preds.is_empty() {
        return;
    }
    let new_facts = facts(run, Some(sid), true);
    match binding::resume_continuation(&preds, &new_facts, n, now()) {
        binding::ContinuationDecision::Continue { binding: nb } => {
            if let Some((old, _)) = preds.iter().find(|(b, _)| b.task_id == nb.task_id) {
                let mut closed = old.clone();
                closed.state = BindingState::Closed;
                closed.end_turn = closed.end_turn.or(Some(closed.start_turn + 1));
                put_binding(tx, &closed);
            }
            put_binding(tx, &nb);
            tx.event(
                "task.binding_changed",
                json!({"task": nb.task_id, "run": run.id, "binding": nb.id}),
                json!({"state": "active", "reason": "resume_continuation"}),
            );
        }
        binding::ContinuationDecision::Ambiguous { candidates } => {
            tx.event(
                "task.binding_changed",
                json!({"run": run.id}),
                json!({"state": "ambiguous", "candidates": candidates, "offers": ["link_run"]}),
            );
        }
        binding::ContinuationDecision::NoMatch => {}
    }
}

pub fn pending_key(run: &str) -> String {
    format!("pending_switch:{run}")
}

fn apply_pending_switches(
    c: &crate::core::Core,
    tx: &mut Tx,
    run: &AgentRun,
    n: u32,
) -> Option<TaskRunBinding> {
    let p = c
        .store
        .kv_get("tracking", &pending_key(&run.id))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<binding::PendingSwitch>(&s).ok())?;
    let Some(cur) = c
        .store
        .find::<TaskRunBinding>(K_BINDING, &p.from_binding)
        .ok()
        .flatten()
        .filter(|b| b.state == BindingState::Active)
    else {
        tx.m.kv("tracking", &pending_key(&run.id), None);
        return None;
    };
    let (closed, opened) = binding::complete_switch(&p, &cur, n, now());
    put_binding(tx, &closed);
    put_binding(tx, &opened);
    tx.m.kv("tracking", &pending_key(&run.id), None);
    tx.event(
        "task.binding_changed",
        json!({"task": opened.task_id, "run": run.id, "binding": opened.id}),
        json!({"state": "active", "from": closed.id, "at_turn": opened.start_turn}),
    );
    Some(closed)
}

/// A run's identity is deterministic when a structured transport reported its native session
/// (04 §3); process detection alone is a suggestion.
pub(crate) fn identity(run: &AgentRun) -> IdentityEvidence {
    IdentityEvidence {
        deterministic: run.harness_session_id.is_some()
            && !matches!(
                run.integration.as_str(),
                "process" | "screen" | "self_report" | ""
            ),
    }
}

// ---- receipts -----------------------------------------------------------------------------------

fn digest(method: &str, p: &Value) -> String {
    let mut p = p.clone();
    if let Some(o) = p.as_object_mut() {
        o.remove("idempotency_key");
    }
    blake3::hash(format!("{method}\n{p}").as_bytes()).to_hex()[..32].to_string()
}

/// A previous outcome for this key, or a conflict when the key was used for something else.
pub fn replay(server: &Server, method: &str, p: &Value) -> Option<R> {
    let key = s(p, "idempotency_key")?;
    let r = server.with_core(|c| c.store.find::<Receipt>(K_RECEIPT, key).ok().flatten())?;
    if r.method != method || r.payload_digest != digest(method, p) {
        return Some(Err(conflict(
            "idempotency_key_reused",
            "this idempotency key was used for a different request",
        )));
    }
    let mut v = r.result;
    v["replayed"] = json!(true);
    Some(Ok(v))
}

pub fn record(tx: &mut Tx, method: &str, p: &Value, result: &Value) {
    if let Some(key) = s(p, "idempotency_key") {
        let r = Receipt {
            key: key.into(),
            method: method.into(),
            payload_digest: digest(method, p),
            result: result.clone(),
            at_ms: now(),
        };
        tx.m.close(K_RECEIPT, key, None, &r);
    }
}

// ---- API ----------------------------------------------------------------------------------------

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        m if ctx.pane_scope.is_some()
            && let Err(e) = scope_check(server, ctx, m, p) =>
        {
            Err(e)
        }
        "task.sources" => sources(server, ctx, p),
        "task.track" => track(server, ctx, p)
            .await
            .inspect(|_| sync_run_tasks(server)),
        "task.detail" => detail(server, p),
        "task.intent.get" => intent_get(server, p),
        "task.intent.update" => intent_update(server, ctx, p),
        "task.bind" => bind(server, ctx, p).inspect(|_| sync_run_tasks(server)),
        "task.unbind" => unbind(server, p).inspect(|_| sync_run_tasks(server)),
        "task.message.prepare" => message_prepare(server, p),
        "task.message.send" => message_send(server, p).await,
        "task.message.get" => message_get(server, p),
        "task.message.cancel" => message_cancel(server, p),
        "task.operation.get" => operation_get(server, ctx, p),
        "task.set" => task_set(server, p),
        _ => return None,
    })
}

/// Authorization before retrieval for pane-scoped callers (15 §11): only tasks and runs in the
/// caller pane's workspace; receipts belong to full-scope callers only.
fn scope_check(
    server: &Server,
    ctx: &Ctx,
    method: &str,
    p: &Value,
) -> Result<(), vk_proto::rpc::RpcError> {
    let Some(scope) = &ctx.pane_scope else {
        return Ok(());
    };
    let denied = || {
        err(
            ErrorKind::PermissionDenied,
            format!("{method}: outside this pane's workspace"),
        )
        .details(json!({"scope": "pane"}))
    };
    if method == "task.operation.get" {
        return Err(denied());
    }
    let my_ws = server
        .with_core(|c| c.pane(scope).map(|x| x.workspace.clone()))
        .ok_or_else(denied)?;
    let ws_of_run = |r: &str| {
        server.with_core(|c| {
            c.run(r)
                .and_then(|r| c.pane(&r.pane))
                .map(|x| x.workspace.clone())
        })
    };
    if let Some(r) = s(p, "run")
        && ws_of_run(r).as_deref() != Some(my_ws.as_str())
    {
        return Err(denied());
    }
    let task = s(p, "task").map(str::to_string).or_else(|| {
        s(p, "message").and_then(|m| {
            server
                .with_core(|c| c.store.find::<TaskMessage>(K_MESSAGE, m).ok().flatten())
                .map(|m| m.task)
        })
    });
    if let Some(t) = task {
        let t = find_task(server, &t)?;
        if t.workspace.as_deref() != Some(my_ws.as_str()) {
            return Err(denied());
        }
    }
    Ok(())
}

pub(crate) fn find_run(
    server: &Server,
    ctx: &Ctx,
    p: &Value,
) -> Result<AgentRun, vk_proto::rpc::RpcError> {
    if let Some(r) = s(p, "run") {
        return server
            .with_core(|c| c.run(r).cloned())
            .ok_or_else(|| not_found("run", r));
    }
    let pane = crate::api::resolve_pane(server, ctx, s(p, "pane").or(Some("@current")))?;
    server
        .with_core(|c| c.run_for_pane(&pane.id).cloned())
        .ok_or_else(|| err(ErrorKind::NotFound, "no agent run in that pane"))
}

pub fn find_task(server: &Server, t: &str) -> Result<Task, vk_proto::rpc::RpcError> {
    server
        .with_core(|c| {
            c.task(t)
                .cloned()
                .or_else(|| c.store.find::<Task>("task", t).ok().flatten())
        })
        .ok_or_else(|| not_found("task", t))
}

/// Selectable source requests for **Track this work**: recent turns of a run with their exact
/// prompt text (the latest is the default selection).
fn sources(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let run = find_run(server, ctx, p)?;
    let turns = turns_of(server, &run.id, u(p, "limit").unwrap_or(10) as usize);
    Ok(json!({
        "run": run.id,
        "harness": run.harness,
        "identity_verified": identity(&run).deterministic,
        "native_conversation_id": run.harness_session_id,
        "turns": turns,
    }))
}

fn criteria_from(p: &Value) -> Vec<DraftCriterion> {
    let one = |v: &Value| -> Option<DraftCriterion> {
        match v {
            Value::String(t) if !t.trim().is_empty() => Some(DraftCriterion {
                id: None,
                text: t.trim().into(),
                required: true,
                evaluation: Evaluation::Human,
                check_definition_ids: vec![],
                source_refs: vec![],
            }),
            Value::Object(_) => Some(DraftCriterion {
                id: s(v, "id").map(str::to_string),
                text: s(v, "text")?.trim().to_string(),
                required: v.get("required").and_then(Value::as_bool).unwrap_or(true),
                evaluation: v
                    .get("evaluation")
                    .and_then(|e| serde_json::from_value(e.clone()).ok())
                    .unwrap_or(if v.get("checks").is_some() {
                        Evaluation::Check
                    } else {
                        Evaluation::Human
                    }),
                check_definition_ids: v
                    .get("checks")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
                source_refs: vec![],
            }),
            _ => None,
        }
    };
    match p.get("criteria").or_else(|| p.get("criterion")) {
        Some(Value::Array(a)) => a.iter().filter_map(one).collect(),
        Some(v) => one(v).into_iter().collect(),
        None => vec![],
    }
}

/// Per item of `criteria`/`constraints` (aligned with [`criteria_from`]/[`constraints_from`]):
/// the `source_turns` it cites — set when the item came from an assistant suggestion whose
/// citations point at turns of the tracked run. `task.track` resolves them against its
/// selected turns; anything else is ignored.
fn cited_turns_of(p: &Value, key: &str, alias: &str) -> Vec<Vec<u32>> {
    let one = |v: &Value| -> Option<Vec<u32>> {
        match v {
            Value::String(t) if !t.trim().is_empty() => Some(vec![]),
            Value::Object(_) => {
                s(v, "text")?;
                Some(
                    v.get("source_turns")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_u64().map(|n| n as u32))
                                .collect()
                        })
                        .unwrap_or_default(),
                )
            }
            _ => None,
        }
    };
    match p.get(key).or_else(|| p.get(alias)) {
        Some(Value::Array(a)) => a.iter().filter_map(one).collect(),
        Some(v) => one(v).into_iter().collect(),
        None => vec![],
    }
}

fn constraints_from(p: &Value) -> Vec<DraftConstraint> {
    let one = |v: &Value| match v {
        Value::String(t) if !t.trim().is_empty() => Some(DraftConstraint {
            id: None,
            text: t.trim().into(),
            source_refs: vec![],
        }),
        Value::Object(_) => Some(DraftConstraint {
            id: s(v, "id").map(str::to_string),
            text: s(v, "text")?.to_string(),
            source_refs: vec![],
        }),
        _ => None,
    };
    match p.get("constraints").or_else(|| p.get("constraint")) {
        Some(Value::Array(a)) => a.iter().filter_map(one).collect(),
        Some(v) => one(v).into_iter().collect(),
        None => vec![],
    }
}

fn stop_from(p: &Value) -> Result<Option<StopAt>, vk_proto::rpc::RpcError> {
    match s(p, "stop_at") {
        None => Ok(None),
        Some(v) => serde_json::from_value(json!(v))
            .map(Some)
            .map_err(|_| invalid(format!("unknown stop_at `{v}`"))),
    }
}

/// `task.track`: atomically create an attached task, confirm intent revision 1 and bind the
/// selected source range. No spawn, send, setup or check (15 §10.2).
async fn track(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    if let Some(r) = replay(server, "task.track", p) {
        return r;
    }
    let run = find_run(server, ctx, p)?;
    if !identity(&run).deterministic {
        return Err(conflict(
            "binding_unverified",
            "this run's identity is not verified (no structured session); link the run first",
        )
        .details(
            json!({"reason": "binding_unverified", "run": run.id, "integration": run.integration}),
        ));
    }
    let turns = turns_of(server, &run.id, 50);
    let selected: Vec<TurnRecord> = match p.get("turns").or_else(|| p.get("turn")) {
        Some(Value::Array(a)) => {
            let want: Vec<u64> = a.iter().filter_map(Value::as_u64).collect();
            turns
                .iter()
                .filter(|t| want.contains(&(t.n as u64)))
                .cloned()
                .collect()
        }
        Some(Value::Number(n)) => turns
            .iter()
            .filter(|t| Some(t.n as u64) == n.as_u64())
            .cloned()
            .collect(),
        _ => turns
            .iter()
            .find(|t| !t.prompt.is_empty())
            .cloned()
            .into_iter()
            .collect(),
    };
    let mut selected = selected;
    selected.sort_by_key(|t| t.n);
    let excerpt = selected
        .iter()
        .map(|t| t.prompt.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let first_line = excerpt
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let title = s(p, "title")
        .map(str::to_string)
        .unwrap_or_else(|| truncate(first_line, 80).0);
    if title.trim().is_empty() {
        return Err(invalid("no source request selected and no title given"));
    }
    let start_turn = selected
        .first()
        .map(|t| t.n)
        .unwrap_or(run.turns_completed + 1);
    let cwd = run
        .cwd
        .clone()
        .or_else(|| server.pane_cwd(&run.pane))
        .unwrap_or_else(|| ".".into());
    // Git work off the state path (15 §5).
    let cwd2 = cwd.clone();
    let target_branch = s(p, "target_branch").map(str::to_string);
    let (repo_root, baseline, base) = tokio::task::spawn_blocking(move || {
        let root = vk_tasks::repo_root(std::path::Path::new(&cwd2)).map(|i| i.root);
        let baseline = root
            .as_ref()
            .and_then(|r| vk_review::subject::observation_baseline(r).ok());
        let base = root.as_ref().and_then(|r| {
            vk_review::subject::propose_review_base(
                r,
                None,
                target_branch.as_deref(),
                crate::review::default_branch(r).as_deref(),
            )
            .ok()
        });
        (root, baseline, base)
    })
    .await
    .map_err(internal)?;
    let pane = server.with_core(|c| c.pane(&run.pane).cloned());
    let actor = user(ctx);
    let mut c = server.core.lock().unwrap();
    let id = crate::core::ulid();
    let handle = c.next_task_handle();
    let source_refs: Vec<SourceRef> = selected
        .iter()
        .map(|t| SourceRef {
            machine: Some(server.opts.machine.clone()),
            session: Some(server.opts.session.clone()),
            run: Some(run.id.clone()),
            native_conversation_id: t.native_conversation_id.clone(),
            turn: Some(t.n),
            item: None,
            digest: Some(blake3::hash(t.prompt.as_bytes()).to_hex()[..16].to_string()),
            delivered_user_message: true,
            provenance: Some(Actor {
                kind: ActorKind::User,
                id: "harness-prompt".into(),
            }),
        })
        .collect();
    // Items citing selected turns (an applied assistant suggestion) carry those turns' refs.
    let cited = |turns: &[u32]| -> Vec<SourceRef> {
        source_refs
            .iter()
            .filter(|r| r.turn.is_some_and(|t| turns.contains(&t)))
            .cloned()
            .collect()
    };
    let cons_turns = cited_turns_of(p, "constraints", "constraint");
    let crit_turns = cited_turns_of(p, "criteria", "criterion");
    let draft = IntentDraft {
        task_id: id.clone(),
        title: title.clone(),
        objective: s(p, "objective").unwrap_or("").to_string(),
        constraints: constraints_from(p)
            .into_iter()
            .zip(
                cons_turns
                    .iter()
                    .map(Vec::as_slice)
                    .chain(std::iter::repeat(&[][..])),
            )
            .map(|(mut c, turns)| {
                c.source_refs = cited(turns);
                c
            })
            .collect(),
        // A criterion quoted verbatim from the delivered request carries that source (15 §2.3).
        criteria: criteria_from(p)
            .into_iter()
            .zip(
                crit_turns
                    .iter()
                    .map(Vec::as_slice)
                    .chain(std::iter::repeat(&[][..])),
            )
            .map(|(mut cr, turns)| {
                let norm = |x: &str| {
                    x.split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .to_lowercase()
                };
                let refs = cited(turns);
                if !refs.is_empty() {
                    cr.source_refs = refs;
                } else if cr.text.len() >= 8 && norm(&excerpt).contains(&norm(&cr.text)) {
                    cr.source_refs = source_refs.clone();
                }
                cr
            })
            .collect(),
        stop_at: stop_from(p)?.unwrap_or_default(),
        stop_detail: s(p, "stop_detail").map(str::to_string),
        source_refs: source_refs.clone(),
        source_excerpt: (!excerpt.is_empty()).then_some(excerpt.clone()),
    };
    let intent = intent::next_revision(None, draft, actor.clone(), now())
        .map_err(|e| invalid(e.to_string()))?;
    let existing = run_bindings(&c, &run.id);
    let b = binding::bind(
        &existing,
        BindRequest {
            task_id: id.clone(),
            run_id: run.id.clone(),
            native_conversation_id: run.harness_session_id.clone().unwrap_or_default(),
            role: BindingRole::Implementation,
            start_turn,
            end_turn: None,
            actor: actor.clone(),
        },
        identity(&run),
        now(),
    )
    .map_err(|e| conflict("binding_conflict", e.to_string()))?;
    let task = Task {
        id: id.clone(),
        handle,
        title: title.clone(),
        slug: vk_tasks::slugify(&title, 40),
        workspace: pane.as_ref().map(|x| x.workspace.clone()),
        repo_root: repo_root
            .as_ref()
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or(cwd.clone()),
        worktree_path: Some(cwd),
        status: "active".into(),
        created_at_ms: now(),
        ownership: TaskOwnership::Attached,
        owner_machine: server.opts.machine.clone(),
        intent_revision: Some(intent.revision),
        rev: 1,
        review_label: Some(
            if intent.stop_at.is_confirmed() && !intent.criteria.is_empty() {
                "in_progress".into()
            } else {
                "needs_task_details".into()
            },
        ),
        ..Default::default()
    };
    // Revision-1 requirements quoted from a delivered message count as communicated.
    let comm: Vec<CommunicationRecord> = if source_refs.is_empty() {
        vec![]
    } else {
        intent
            .criteria
            .iter()
            .filter(|cr| cr.source_refs.iter().any(|r| r.delivered_user_message))
            .map(|cr| CommunicationRecord {
                criterion_id: cr.id.clone(),
                version: cr.version,
                native_conversation_id: b.native_conversation_id.clone(),
                via: CommunicationVia::DeliveredMessage {
                    message_id: format!(
                        "turn:{}",
                        cr.source_refs
                            .iter()
                            .filter_map(|r| r.turn)
                            .map(|n| n.to_string())
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                },
            })
            .collect()
    };
    let mut tx = Tx::new();
    tx.task(task.clone());
    tx.m.close(
        K_INTENT,
        &format!("{id}:{}", intent.revision),
        None,
        &intent,
    );
    put_binding(&mut tx, &b);
    tx.m.put(K_COMM, &id, None, &comm);
    tx.m.put(
        K_BASELINE,
        &id,
        None,
        &json!({"baseline": baseline, "review_base": base}),
    );
    tx.event_by(
        "task.tracked",
        json!({"task": id, "run": run.id}),
        json!({"kind": "user", "id": actor.id}),
        json!({"intent_revision": intent.revision, "binding": b.id, "start_turn": start_turn}),
    );
    let result = json!({"task": task, "intent": intent, "binding": b, "baseline": baseline, "review_base": base});
    record(&mut tx, "task.track", p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(result)
}

pub fn intent_at(server: &Server, task: &str, rev: u32) -> Option<TaskIntent> {
    server.with_core(|c| {
        c.store
            .find::<TaskIntent>(K_INTENT, &format!("{task}:{rev}"))
            .ok()
            .flatten()
    })
}

/// The observation baseline and proposed review base stored by `task.track` (15 §5).
pub fn baseline_of(server: &Server, task: &str) -> Option<Value> {
    server.with_core(|c| c.store.find::<Value>(K_BASELINE, task).ok().flatten())
}

fn comm_of(server: &Server, task: &str) -> Vec<CommunicationRecord> {
    server
        .with_core(|c| {
            c.store
                .find::<Vec<CommunicationRecord>>(K_COMM, task)
                .ok()
                .flatten()
        })
        .unwrap_or_default()
}

pub fn task_bindings(server: &Server, task: &str) -> Vec<TaskRunBinding> {
    server.with_core(|c| {
        sorted(
            c.store
                .load_by_field(K_BINDING, "$.task_id", task)
                .unwrap_or_default(),
        )
    })
}

pub fn messages_of(server: &Server, task: &str) -> Vec<TaskMessage> {
    server.with_core(|c| {
        let mut v: Vec<TaskMessage> = c.store.load_by_task(K_MESSAGE, task).unwrap_or_default();
        v.sort_by_key(|m| m.created_at_ms);
        v
    })
}

fn uncommunicated(server: &Server, task: &str, intent: &TaskIntent) -> Vec<String> {
    let conv = task_bindings(server, task)
        .into_iter()
        .rev()
        .find(|b| b.state == BindingState::Active)
        .map(|b| b.native_conversation_id);
    match conv {
        Some(conv) => intent::uncommunicated(intent, &comm_of(server, task), &conv),
        None => intent.criteria.iter().map(|c| c.id.clone()).collect(),
    }
}

fn intent_get(server: &Server, p: &Value) -> R {
    let task = find_task(server, req(p, "task")?)?;
    let Some(cur) = task.intent_revision else {
        return Ok(json!({"task": task.id, "intent": null, "revisions": []}));
    };
    let rev = u(p, "revision").map(|r| r as u32).unwrap_or(cur);
    let intent = intent_at(server, &task.id, rev)
        .ok_or_else(|| not_found("intent revision", &rev.to_string()))?;
    let uncomm = uncommunicated(server, &task.id, &intent);
    Ok(
        json!({"task": task.id, "current_revision": cur, "intent": intent, "revisions": (1..=cur).collect::<Vec<_>>(), "uncommunicated": uncomm}),
    )
}

/// Record-only: a new immutable revision. Never sends (15 §2.3).
fn intent_update(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    if let Some(r) = replay(server, "task.intent.update", p) {
        return r;
    }
    let task = find_task(server, req(p, "task")?)?;
    let cur = task
        .intent_revision
        .ok_or_else(|| conflict("not_tracked", "task has no intent yet; use task.track"))?;
    if let Some(exp) = u(p, "expected_revision")
        && exp as u32 != cur
    {
        return Err(conflict(
            "review_changed",
            format!("intent is at revision {cur}, not {exp}"),
        )
        .details(json!({"reason": "intent_revision_changed", "current": cur})));
    }
    let prev =
        intent_at(server, &task.id, cur).ok_or_else(|| internal("missing intent revision"))?;
    let mut draft = IntentDraft::from_intent(&prev);
    if let Some(t) = s(p, "title") {
        draft.title = t.into();
    }
    if let Some(o) = s(p, "objective") {
        draft.objective = o.into();
    }
    if p.get("criteria").is_some() || p.get("criterion").is_some() {
        draft.criteria = criteria_from(p);
    }
    if let Some(add) = p.get("add_criterion") {
        draft
            .criteria
            .extend(criteria_from(&json!({"criteria": [add]})));
    }
    if p.get("constraints").is_some() || p.get("constraint").is_some() {
        draft.constraints = constraints_from(p);
    }
    if let Some(sa) = stop_from(p)? {
        draft.stop_at = sa;
    }
    if let Some(d) = s(p, "stop_detail") {
        draft.stop_detail = Some(d.into());
    }
    let next = intent::next_revision(Some(&prev), draft, user(ctx), now())
        .map_err(|e| invalid(e.to_string()))?;
    let mut t2 = task.clone();
    t2.intent_revision = Some(next.revision);
    t2.title = next.title.clone();
    t2.rev += 1;
    if t2.review_label.as_deref() == Some("needs_task_details")
        && next.stop_at.is_confirmed()
        && !next.criteria.is_empty()
    {
        t2.review_label = Some("in_progress".into());
    }
    let uncomm = uncommunicated(server, &task.id, &next);
    let mut c = server.core.lock().unwrap();
    // Recheck under the state lock: a concurrent edit must never overwrite a revision.
    let now_rev = c.task(&task.id).and_then(|t| t.intent_revision);
    let taken = c
        .store
        .find::<TaskIntent>(K_INTENT, &format!("{}:{}", task.id, next.revision))
        .ok()
        .flatten()
        .is_some();
    if now_rev != Some(cur) || taken {
        return Err(conflict(
            "review_changed",
            "the intent changed while you were editing; reload",
        )
        .details(json!({"reason": "intent_revision_changed", "current": now_rev})));
    }
    let mut tx = Tx::new();
    tx.task(t2.clone());
    tx.m.close(
        K_INTENT,
        &format!("{}:{}", task.id, next.revision),
        None,
        &next,
    );
    tx.event(
        "task.intent_updated",
        json!({"task": task.id}),
        json!({"revision": next.revision, "previous": cur}),
    );
    let result = json!({"task": t2, "intent": next, "uncommunicated": uncomm, "note": if uncomm.is_empty() { Value::Null } else { json!(format!("Agent has not been told about revision {}", next.revision)) }});
    record(&mut tx, "task.intent.update", p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    // A new intent revision can outdate an acceptance and change readiness (15 §7).
    crate::review::spawn_refresh(server, &task.id);
    Ok(result)
}

/// Explicit verified association; with an active binding to another task on the same run the
/// switch takes effect at the next turn boundary (queued while a turn runs).
fn bind(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    if let Some(r) = replay(server, "task.bind", p) {
        return r;
    }
    let task = find_task(server, req(p, "task")?)?;
    let run = find_run(server, ctx, p)?;
    if !identity(&run).deterministic {
        return Err(conflict(
            "binding_unverified",
            "run identity is not verified; suggested bindings can't attach work",
        ));
    }
    let role: BindingRole = s(p, "role")
        .and_then(|r| serde_json::from_value(json!(r)).ok())
        .unwrap_or(BindingRole::Implementation);
    let actor = user(ctx);
    let mut c = server.core.lock().unwrap();
    let all = run_bindings(&c, &run.id);
    let mut tx = Tx::new();
    let mut newly_closed = vec![];
    let current = all
        .iter()
        .find(|b| b.run_id == run.id && b.state == BindingState::Active && b.role == role);
    let result = if let Some(cur) = current.filter(|b| b.task_id != task.id) {
        let pos = if run.execution.value == Execution::Working {
            binding::TurnPosition::Running {
                turn: run.turns_completed + 1,
            }
        } else {
            binding::TurnPosition::Idle {
                next_turn: run.turns_completed + 1,
            }
        };
        match binding::queue_switch(cur, &task.id, role, pos, actor, now()) {
            binding::SwitchPlan::Immediate { closed, opened } => {
                put_binding(&mut tx, &closed);
                put_binding(&mut tx, &opened);
                tx.event(
                    "task.binding_changed",
                    json!({"task": task.id, "run": run.id, "binding": opened.id}),
                    json!({"state": "active", "from": closed.id}),
                );
                newly_closed.push(closed.clone());
                json!({"binding": opened, "closed": closed, "pending": false})
            }
            binding::SwitchPlan::Pending(ps) => {
                tx.m.kv(
                    "tracking",
                    &pending_key(&run.id),
                    Some(serde_json::to_string(&ps).unwrap_or_default()),
                );
                tx.event(
                    "task.binding_changed",
                    json!({"task": task.id, "run": run.id}),
                    json!({"state": "pending", "after_turn": ps.after_turn}),
                );
                json!({"pending": true, "switch": ps})
            }
        }
    } else if let Some(sus) = all
        .iter()
        .find(|b| b.run_id == run.id && b.task_id == task.id && b.state == BindingState::Suspended)
    {
        // Continue task after a conversation boundary.
        let nb = binding::continue_after_boundary(
            sus,
            run.harness_session_id.as_deref().unwrap_or_default(),
            run.turns_completed + 1,
            actor,
            now(),
        );
        let mut closed = sus.clone();
        closed.state = BindingState::Closed;
        put_binding(&mut tx, &closed);
        put_binding(&mut tx, &nb);
        tx.event(
            "task.binding_changed",
            json!({"task": task.id, "run": run.id, "binding": nb.id}),
            json!({"state": "active", "continued_from": sus.id}),
        );
        json!({"binding": nb, "pending": false})
    } else {
        let b = binding::bind(
            &all,
            BindRequest {
                task_id: task.id.clone(),
                run_id: run.id.clone(),
                native_conversation_id: run.harness_session_id.clone().unwrap_or_default(),
                role,
                start_turn: u(p, "start_turn")
                    .map(|n| n as u32)
                    .unwrap_or(run.turns_completed + 1),
                end_turn: u(p, "end_turn").map(|n| n as u32),
                actor,
            },
            identity(&run),
            now(),
        )
        .map_err(|e| conflict("binding_conflict", e.to_string()))?;
        put_binding(&mut tx, &b);
        tx.event(
            "task.binding_changed",
            json!({"task": task.id, "run": run.id, "binding": b.id}),
            json!({"state": "active"}),
        );
        json!({"binding": b, "pending": false})
    };
    record(&mut tx, "task.bind", p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    if !newly_closed.is_empty() {
        crate::review::on_bindings_closed(server, newly_closed);
    }
    Ok(result)
}

fn unbind(server: &Arc<Server>, p: &Value) -> R {
    if let Some(r) = replay(server, "task.unbind", p) {
        return r;
    }
    let task = find_task(server, req(p, "task")?)?;
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    let mut closed = vec![];
    for mut b in sorted(
        c.store
            .load_by_field(K_BINDING, "$.task_id", &task.id)
            .unwrap_or_default(),
    )
    .into_iter()
    .filter(|b| {
        b.task_id == task.id
            && b.state != BindingState::Closed
            && s(p, "binding").is_none_or(|x| x == b.id)
    }) {
        let end = c
            .run(&b.run_id)
            .map(|r| r.turns_completed + 1)
            .unwrap_or(b.start_turn + 1);
        b.end_turn = Some(end.max(b.start_turn + 1));
        b.state = BindingState::Closed;
        put_binding(&mut tx, &b);
        tx.event(
            "task.binding_changed",
            json!({"task": task.id, "run": b.run_id, "binding": b.id}),
            json!({"state": "closed"}),
        );
        closed.push(b);
    }
    let result = json!({"closed": closed});
    record(&mut tx, "task.unbind", p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    crate::review::on_bindings_closed(server, closed);
    Ok(result)
}

fn detail(server: &Server, p: &Value) -> R {
    let task = find_task(server, req(p, "task")?)?;
    let intent = task
        .intent_revision
        .and_then(|r| intent_at(server, &task.id, r));
    let bs = task_bindings(server, &task.id);
    let baseline = server.with_core(|c| c.store.find::<Value>(K_BASELINE, &task.id).ok().flatten());
    let uncomm = intent
        .as_ref()
        .map(|i| uncommunicated(server, &task.id, i))
        .unwrap_or_default();
    let runs: Vec<Value> = bs
        .iter()
        .map(|b| b.run_id.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .filter_map(|r| server.with_core(|c| c.run(&r).cloned().or_else(|| c.store.find::<AgentRun>("run", &r).ok().flatten())))
        .map(|r| json!({"id": r.id, "handle": r.handle, "harness": r.harness, "pane": r.pane, "execution": r.execution.value, "ended": r.ended_at_ms.is_some()}))
        .collect();
    Ok(json!({
        "task": task,
        "intent": intent,
        "bindings": bs,
        "runs": runs,
        "baseline": baseline,
        "uncommunicated": uncomm,
        "messages": messages_of(server, &task.id),
    }))
}

fn task_set(server: &Server, p: &Value) -> R {
    if let Some(r) = replay(server, "task.set", p) {
        return r;
    }
    let mut task = find_task(server, req(p, "task")?)?;
    if let Some(exp) = u(p, "expected_rev")
        && exp != task.rev
    {
        return Err(conflict(
            "task_changed",
            format!("task is at rev {}, not {exp}", task.rev),
        ));
    }
    if let Some(pr) = p.get("priority") {
        task.priority = pr.as_i64().map(|x| x as i32);
    }
    if let Some(e) = s(p, "effort") {
        if !matches!(e, "quick" | "minutes" | "deep" | "unknown") {
            return Err(invalid("effort is quick | minutes | deep | unknown"));
        }
        task.effort = Some(e.into());
    }
    task.rev += 1;
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.task(task.clone());
    tx.event(
        "task.updated",
        json!({"task": task.id}),
        // `effort_source`: who proposed the applied value (user | heuristic | assistant:<request>);
        // applying it is always this explicit user call (15 §8.2, T4).
        json!({"priority": task.priority, "effort": task.effort, "effort_source": s(p, "effort").map(|_| s(p, "effort_source").unwrap_or("user"))}),
    );
    let result = json!({"task": task});
    record(&mut tx, "task.set", p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(result)
}

/// Attached-task lifecycle: only the record changes (15 §4.3).
pub fn finish_attached(server: &Server, task: &Task, status: &str) -> R {
    if !matches!(status, "finished" | "parked" | "archived" | "active") {
        return Err(invalid("status is finished | parked | archived | active"));
    }
    let mut t = task.clone();
    t.status = status.into();
    t.rev += 1;
    let accepted = matches!(
        t.review_label.as_deref(),
        Some("reviewed") | Some("reviewed_with_exceptions")
    );
    if status == "finished" && !accepted {
        t.review_label = Some("finished_without_review".into());
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.task(t.clone());
    tx.event(
        "task.finished",
        json!({"task": t.id}),
        json!({"status": status, "attached": true, "reviewed": accepted}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(
        json!({"task": t, "note": "attached task: processes, workspace, files and ports are unchanged"}),
    )
}

fn operation_get(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let key = req(p, "idempotency_key")?;
    match crate::review::receipts::lookup(server, ctx, key) {
        Some(r) if now() - r.at_ms <= RECEIPT_WINDOW_MS => {
            Ok(json!({"known": true, "method": r.method, "result": r.result, "at_ms": r.at_ms}))
        }
        Some(_) => Ok(
            json!({"known": false, "expired": true, "note": "receipt expired: inspect current state before retrying"}),
        ),
        None => Ok(
            json!({"known": false, "note": "no receipt: this does not mean the operation is safe to repeat; inspect current state"}),
        ),
    }
}

// ---- messages (15 §9) ---------------------------------------------------------------------------

fn active_binding(server: &Server, task: &str) -> Option<TaskRunBinding> {
    task_bindings(server, task)
        .into_iter()
        .rev()
        .find(|b| b.state == BindingState::Active)
}

fn put_message(tx: &mut Tx, m: &TaskMessage) {
    if matches!(m.state, MessageState::Delivered | MessageState::Cancelled) {
        tx.m.close(K_MESSAGE, &m.id, None, m);
    } else {
        tx.m.put(K_MESSAGE, &m.id, None, m);
    }
}

fn message_prepare(server: &Server, p: &Value) -> R {
    if let Some(r) = replay(server, "task.message.prepare", p) {
        return r;
    }
    let task = find_task(server, req(p, "task")?)?;
    let text = req(p, "text")?;
    let b = active_binding(server, &task.id).ok_or_else(|| {
        conflict(
            "binding_changed",
            "task has no active run binding; choose a recipient",
        )
    })?;
    let intent = task
        .intent_revision
        .and_then(|r| intent_at(server, &task.id, r));
    let covers: Vec<(String, u32)> = if p
        .get("communicates_intent")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        intent
            .as_ref()
            .map(|i| {
                i.criteria
                    .iter()
                    .map(|c| (c.id.clone(), c.version))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![]
    };
    let m = TaskMessage {
        id: crate::core::ulid(),
        task: task.id.clone(),
        binding: b.id.clone(),
        run: b.run_id.clone(),
        native_conversation_id: b.native_conversation_id.clone(),
        intent_revision: task.intent_revision,
        text: text.into(),
        state: MessageState::Prepared,
        detail: None,
        covers,
        idempotency_key: s(p, "idempotency_key").map(str::to_string),
        created_at_ms: now(),
        updated_at_ms: now(),
    };
    let safety = send_safety(server, &m);
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    put_message(&mut tx, &m);
    tx.event(
        "task.message_prepared",
        json!({"task": task.id, "message": m.id}),
        json!({"run": m.run}),
    );
    let result = json!({"message": m, "recipient": {"run": m.run, "binding": m.binding}, "send_path": safety.as_ref().map(|_| "prompt_input").unwrap_or("open_pane_only"), "unsafe": safety.err()});
    record(&mut tx, "task.message.prepare", p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(result)
}

/// The text in the agent's input box: `Some("")` only when the prompt line *and* every
/// continuation line inside the box are empty. Placeholder hints count as text: refusing is
/// the safe failure (the user gets "Open pane to send").
fn input_text(screen: &str) -> Option<String> {
    let lines: Vec<&str> = screen.lines().collect();
    let start = lines.len().saturating_sub(30);
    let strip = |l: &str| {
        l.trim()
            .trim_matches(|c| c == '│' || c == '┃' || c == '|')
            .trim()
            .to_string()
    };
    for i in (start..lines.len()).rev() {
        let t = strip(lines[i]);
        let Some(rest) = ["> ", "›", ">"].iter().find_map(|pre| t.strip_prefix(pre)) else {
            continue;
        };
        let mut text = rest.trim().to_string();
        for l in &lines[i + 1..] {
            let raw = l.trim();
            if raw.starts_with('╰') || raw.starts_with('└') || raw.starts_with('─') {
                break;
            }
            let more = strip(l);
            if !more.is_empty() {
                text.push('\n');
                text.push_str(&more);
            }
        }
        return Some(text);
    }
    None
}

/// Every reason the prompt-input path is unsafe right now; `Ok` means it may be attempted.
fn send_safety(server: &Server, m: &TaskMessage) -> Result<(), String> {
    server
        .with_core(|c| c.run(&m.run).cloned())
        .ok_or("the target run has ended")?;
    let b = active_binding(server, &m.task).ok_or("the task has no active binding")?;
    if b.id != m.binding || b.run_id != m.run {
        return Err("the task moved to another run or binding".into());
    }
    prompt_input_safety(server, &m.run, &m.native_conversation_id)
}

/// The task-independent part of [`send_safety`] (15 §9), shared with the drafts composer: the
/// run is live, still in `conversation`, idle, has no open interaction, no attached client
/// focuses its pane, and its input box is provably empty.
pub(crate) fn prompt_input_safety(
    server: &Server,
    run_id: &str,
    conversation: &str,
) -> Result<(), String> {
    let run = server
        .with_core(|c| c.run(run_id).cloned())
        .ok_or("the target run has ended")?;
    if run.harness_session_id.as_deref() != Some(conversation) {
        return Err("the run is in a different conversation now".into());
    }
    if run.execution.value != Execution::Idle {
        return Err(format!("the agent is {:?}, not idle", run.execution.value).to_lowercase());
    }
    if server.with_core(|c| {
        c.model
            .interactions
            .iter()
            .any(|i| i.pane == run.pane && i.status == InteractionStatus::Open)
    }) {
        return Err("a question or approval is open in that pane".into());
    }
    if server
        .clients
        .lock()
        .unwrap()
        .values()
        .any(|cl| cl.focus.pane.as_deref() == Some(run.pane.as_str()) && cl.kind == "tui")
    {
        return Err("an attached client is focused on that pane (the user may be typing)".into());
    }
    let rt = server.pane_rt(&run.pane).ok_or("the pane is gone")?;
    let screen = rt.screen.lock().unwrap().engine.screen_text();
    match input_text(&screen) {
        Some(t) if t.is_empty() => Ok(()),
        Some(_) => Err("the agent's input box has a draft in it".into()),
        None => Err("can't establish that the agent's input is empty".into()),
    }
}

async fn message_send(server: &Arc<Server>, p: &Value) -> R {
    // A repeated request with the same key reports the message as it is now (15 §10.3).
    if let Some(r) = replay(server, "task.message.send", p) {
        let id = req(p, "message")?;
        return r
            .and_then(|_| message_get(server, &json!({"message": id})))
            .map(|mut v| {
                v["replayed"] = json!(true);
                v
            });
    }
    let id = req(p, "message")?;
    let m = server
        .with_core(|c| c.store.find::<TaskMessage>(K_MESSAGE, id).ok().flatten())
        .ok_or_else(|| not_found("message", id))?;
    match m.state {
        MessageState::Prepared => {}
        MessageState::Failed if b_flag(p, "retry") => {}
        MessageState::DeliveryUnknown if b_flag(p, "retry_despite_unknown") => {}
        MessageState::DeliveryUnknown => {
            return Err(conflict(
                "delivery_unknown",
                "the earlier attempt may have arrived; pass retry_despite_unknown to send again",
            ));
        }
        _ => return Ok(json!({"message": m})),
    }
    if let Err(why) = send_safety(server, &m) {
        return Err(err(ErrorKind::Conflict, format!("not sent (zero bytes written): {why}")).details(json!({"reason": "send_unsafe", "detail": why, "fallback": "open_pane_to_send", "pane": server.with_core(|c| c.run(&m.run).map(|r| r.pane.clone()))})));
    }
    if !server
        .tracking
        .inflight
        .lock()
        .unwrap()
        .insert(m.id.clone())
    {
        return Err(conflict(
            "send_in_progress",
            "this message is already being sent",
        ));
    }
    // Conditional transition: only from the state we validated, under the state lock.
    let m = {
        let mut c = server.core.lock().unwrap();
        let cur = c.store.find::<TaskMessage>(K_MESSAGE, &m.id).ok().flatten();
        if cur.as_ref().map(|x| x.state) != Some(m.state) {
            drop(c);
            server.tracking.inflight.lock().unwrap().remove(&m.id);
            return Err(conflict(
                "message_changed",
                "the message changed state; reload it",
            ));
        }
        let mut m = m;
        m.state = MessageState::Sending;
        m.updated_at_ms = now();
        let mut tx = Tx::new();
        put_message(&mut tx, &m);
        tx.event(
            "task.message_sending",
            json!({"task": m.task, "message": m.id}),
            json!({}),
        );
        record(&mut tx, "task.message.send", p, &json!({"message": m.id}));
        if let Err(e) = server.commit(&mut c, tx) {
            drop(c);
            server.tracking.inflight.lock().unwrap().remove(&m.id);
            return Err(internal(e));
        }
        m
    };
    let srv = server.clone();
    let m2 = m.clone();
    tokio::spawn(async move {
        let (state, detail) = deliver(&srv, &m2).await;
        srv.tracking.inflight.lock().unwrap().remove(&m2.id);
        let mut m3 = m2.clone();
        m3.state = state;
        m3.detail = detail;
        m3.updated_at_ms = now();
        let mut c = srv.core.lock().unwrap();
        let mut tx = Tx::new();
        put_message(&mut tx, &m3);
        if state == MessageState::Delivered && !m3.covers.is_empty() {
            let mut comm: Vec<CommunicationRecord> = c
                .store
                .find(K_COMM, &m3.task)
                .ok()
                .flatten()
                .unwrap_or_default();
            for (cid, ver) in &m3.covers {
                comm.push(CommunicationRecord {
                    criterion_id: cid.clone(),
                    version: *ver,
                    native_conversation_id: m3.native_conversation_id.clone(),
                    via: CommunicationVia::DeliveredMessage {
                        message_id: m3.id.clone(),
                    },
                });
            }
            tx.m.put(K_COMM, &m3.task, None, &comm);
        }
        let kind = match state {
            MessageState::Delivered => "task.message_delivered",
            MessageState::DeliveryUnknown => "task.message_delivery_unknown",
            _ => "task.message_failed",
        };
        tx.event(
            kind,
            json!({"task": m3.task, "message": m3.id}),
            json!({"detail": m3.detail}),
        );
        let _ = srv.commit(&mut c, tx);
    });
    Ok(json!({"message": m}))
}

struct DeliveryTarget<'a> {
    run: &'a str,
    native_conversation_id: &'a str,
    text: &'a str,
}

fn b_flag(p: &Value, k: &str) -> bool {
    p.get(k).and_then(Value::as_bool).unwrap_or(false)
}

/// Prompt-input delivery with a final recheck; delivered only when a bound turn with this text
/// starts on the same run and conversation.
async fn deliver(server: &Arc<Server>, m: &TaskMessage) -> (MessageState, Option<String>) {
    if let Err(why) = send_safety(server, m) {
        return (MessageState::Failed, Some(format!("not sent: {why}")));
    }
    deliver_text(server, &m.run, &m.native_conversation_id, &m.text).await
}

/// Whether a turn numbered `n0` or later started on `run` in `conversation` with `text` in its
/// prompt (whitespace-normalized): the only evidence that counts as delivery (15 §9).
pub(crate) fn turn_matches(
    server: &Server,
    run: &str,
    conversation: &str,
    n0: u32,
    text: &str,
) -> bool {
    let want: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    turns_of(server, run, 5).into_iter().any(|t| {
        t.n >= n0
            && t.native_conversation_id.as_deref() == Some(conversation)
            && t.prompt
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains(&want)
    })
}

/// The prompt-input delivery itself (caller has just checked safety): paste under the pane's
/// input lock, recheck, submit, then wait for a matching turn. Shared by task messages and
/// drafts.
pub(crate) async fn deliver_text(
    server: &Arc<Server>,
    run_id: &str,
    conversation: &str,
    text: &str,
) -> (MessageState, Option<String>) {
    let m = DeliveryTarget {
        run: run_id,
        native_conversation_id: conversation,
        text,
    };
    let Some(run) = server.with_core(|c| c.run(m.run).cloned()) else {
        return (MessageState::Failed, Some("run ended".into()));
    };
    let n0 = run.turns_completed + 1;
    let modes = crate::render::input_modes(server, &run.pane);
    let bytes = if modes.bracketed_paste {
        vk_term::encode::encode_paste(m.text, &modes)
    } else {
        m.text.replace('\n', " ").into_bytes()
    };
    // Hold the pane's input lock from paste to Enter so no client or API caller can interleave.
    server.agents.lock_input(&run.pane);
    let st =
        crate::render::write_and_ack(server, &run.pane, server.next_internal_input_id(), bytes)
            .await;
    if st != vk_proto::holder::InputStatus::Written
        && st != vk_proto::holder::InputStatus::Duplicate
    {
        server.agents.unlock_input(&run.pane);
        return (
            MessageState::DeliveryUnknown,
            Some(format!(
                "the paste was not confirmed ({st:?}); nothing was submitted"
            )),
        );
    }
    tokio::time::sleep(Duration::from_millis(80)).await;
    // Recheck before submitting: still idle, nothing opened, nobody focused, and the input box
    // holds exactly our text (never a user draft merged with it).
    let ok =
        {
            let focus_free =
                !server.clients.lock().unwrap().values().any(|cl| {
                    cl.focus.pane.as_deref() == Some(run.pane.as_str()) && cl.kind == "tui"
                });
            let quiet = server.with_core(|c| {
                c.run(m.run)
                    .is_some_and(|r| r.execution.value == Execution::Idle)
                    && !c
                        .model
                        .interactions
                        .iter()
                        .any(|i| i.pane == run.pane && i.status == InteractionStatus::Open)
            });
            let shown = server
                .pane_rt(&run.pane)
                .and_then(|rt| input_text(&rt.screen.lock().unwrap().engine.screen_text()));
            let norm = |x: &str| x.split_whitespace().collect::<Vec<_>>().join(" ");
            focus_free
                && quiet
                && shown.is_some_and(|t| {
                    norm(&t) == norm(m.text) || norm(m.text).starts_with(&norm(&t)) && !t.is_empty()
                })
        };
    if !ok {
        server.agents.unlock_input(&run.pane);
        return (MessageState::DeliveryUnknown, Some("conditions changed after pasting; the text may be in the input box but was not submitted — check the pane".into()));
    }
    crate::render::write_and_ack(
        server,
        &run.pane,
        server.next_internal_input_id(),
        b"\r".to_vec(),
    )
    .await;
    server.agents.unlock_input(&run.pane);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if turn_matches(server, m.run, m.native_conversation_id, n0, m.text) {
            return (MessageState::Delivered, None);
        }
    }
    (
        MessageState::DeliveryUnknown,
        Some("no matching turn started within 10 s; check the pane before retrying".into()),
    )
}

fn message_get(server: &Server, p: &Value) -> R {
    let id = req(p, "message")?;
    let mut m = server
        .with_core(|c| c.store.find::<TaskMessage>(K_MESSAGE, id).ok().flatten())
        .ok_or_else(|| not_found("message", id))?;
    // A send interrupted by a server restart has an unknown outcome; never assume either way.
    if m.state == MessageState::Sending && !server.tracking.inflight.lock().unwrap().contains(&m.id)
    {
        m.state = MessageState::DeliveryUnknown;
        m.detail = Some("the server restarted during delivery".into());
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        put_message(&mut tx, &m);
        tx.event(
            "task.message_delivery_unknown",
            json!({"task": m.task, "message": m.id}),
            json!({"detail": m.detail}),
        );
        server.commit(&mut c, tx).map_err(internal)?;
    }
    Ok(json!({"message": m}))
}

fn message_cancel(server: &Server, p: &Value) -> R {
    if let Some(r) = replay(server, "task.message.cancel", p) {
        return r;
    }
    let id = req(p, "message")?;
    let mut m = server
        .with_core(|c| c.store.find::<TaskMessage>(K_MESSAGE, id).ok().flatten())
        .ok_or_else(|| not_found("message", id))?;
    if !matches!(m.state, MessageState::Prepared | MessageState::Failed) {
        return Err(conflict(
            "message_state",
            format!("message is {:?}", m.state).to_lowercase(),
        ));
    }
    m.state = MessageState::Cancelled;
    m.updated_at_ms = now();
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    put_message(&mut tx, &m);
    let result = json!({"message": m});
    record(&mut tx, "task.message.cancel", p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(result)
}

/// Startup: sends that were in flight when the server died become `delivery_unknown`.
pub fn recover(server: &Server) {
    let stale: Vec<TaskMessage> = server
        .with_core(|c| c.store.load::<TaskMessage>(K_MESSAGE).unwrap_or_default())
        .into_iter()
        .filter(|m| m.state == MessageState::Sending)
        .collect();
    if stale.is_empty() {
        return;
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    for mut m in stale {
        m.state = MessageState::DeliveryUnknown;
        m.detail = Some("the server restarted during delivery".into());
        put_message(&mut tx, &m);
        tx.event(
            "task.message_delivery_unknown",
            json!({"task": m.task, "message": m.id}),
            json!({"detail": m.detail}),
        );
    }
    let _ = server.commit(&mut c, tx);
}

/// For tests and the TUI: bindings by task id.
pub fn bindings_by_task(server: &Server) -> HashMap<String, Vec<TaskRunBinding>> {
    let mut m: HashMap<String, Vec<TaskRunBinding>> = HashMap::new();
    for b in server.with_core(|c| bindings(c)) {
        m.entry(b.task_id.clone()).or_default().push(b);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drafts_are_never_empty() {
        // A draft that happens to start like Claude's placeholder is still a draft.
        assert_eq!(
            input_text("│ > Try \"fix the bug\" │").as_deref(),
            Some("Try \"fix the bug\"")
        );
        // Multiline drafts: continuation lines inside the box count.
        let t = input_text("╭────╮\n│ > \n│   second line of my draft\n╰────╯").unwrap();
        assert!(!t.is_empty());
    }

    #[test]
    fn input_box_detection() {
        assert_eq!(input_text("╭────╮\n│ > \n╰────╯").as_deref(), Some(""));
        assert_eq!(
            input_text("│ > half typed │").as_deref(),
            Some("half typed")
        );
        assert_eq!(input_text("› ").as_deref(), Some(""));
        assert_eq!(input_text("no prompt here"), None);
    }

    #[test]
    fn shell_commands_only_for_shell_tools() {
        assert_eq!(
            shell_command("Bash", &json!({"command": "cargo test"})).as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            shell_command("exec_command", &json!({"cmd": ["npm", "test"]})).as_deref(),
            Some("npm test")
        );
        assert!(shell_command("Edit", &json!({"file_path": "x"})).is_none());
    }

    #[test]
    fn receipt_digest_ignores_key() {
        assert_eq!(
            digest("m", &json!({"a": 1, "idempotency_key": "x"})),
            digest("m", &json!({"a": 1, "idempotency_key": "y"}))
        );
        assert_ne!(digest("m", &json!({"a": 1})), digest("m", &json!({"a": 2})));
    }
}
