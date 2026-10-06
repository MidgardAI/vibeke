//! The Turn/Item stream (02 §1.1; 3D): a structured record of what each run did, modelled after
//! Codex app-server's Thread/Turn/Item and ACP so mapping is lossless.
//!
//! ```text
//! Turn { id, run_id, seq, started_at, ended_at?, input_summary, status, usage?, item_count }
//! Item { id, turn_id, seq, kind, started_at, ended_at?, summary, payload_ref?, file_change? }
//! ```
//!
//! Turns and items are state (`stream_turn`, `stream_item` entities); every item also emits one
//! `agent.item {kind, summary, item, turn, seq}` event (sync tier; at item granularity, never per
//! token). Large payloads (tool output, long messages) are stored once in the unified blob store
//! (`blob_store::put_payload`, `source: payload`) and referenced by `payload_ref`; the event
//! never carries them. Everything stored passes `vk_redact` first: summaries and payloads are
//! excerpts, not transcripts (the tracking `turn` records of 15 hold exact prompts for tracked
//! tasks only).
//!
//! Transport-neutral recording is [`turn_started`], [`item`], [`item_finished`],
//! [`turn_ended`] and [`usage_updated`]; [`observe_hook`] maps the Claude/Codex hook vocabulary
//! onto them (`on_signal`). Other transports (headless adapters, ACP, extensions) call the same
//! functions. `agent.turns` and `agent.items` read the stream (full scope only).
//!
//! `file_change` items feed the collision tracker (05) and Phase 2 evidence bundles; the event
//! `agent.file_changed` stays as the cheap per-path signal.

use crate::Server;
use crate::api::{R, err, invalid, not_found, s, u};
use crate::core::{Core, Tx};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use vk_proto::entities::{FileChange, FileOp, Item, ItemKind, Turn, TurnStatus, TurnUsage};
use vk_proto::model::{AgentRun, RunUsage};
use vk_proto::rpc::ErrorKind;
use vk_store::now_ms;

pub const METHODS: &[(&str, bool)] = &[("agent.turns", false), ("agent.items", false)];

/// Items carry tool summaries and message excerpts: full scope only.
pub const PANE_FORBIDDEN: &[&str] = &["agent.turns", "agent.items"];

const K_TURN: &str = "stream_turn";
const K_ITEM: &str = "stream_item";

/// Longest stored summary (characters).
pub const SUMMARY_MAX: usize = 200;
/// A payload at most this long stays out of the blob store (its summary already holds it).
pub const INLINE_MAX: usize = 2048;
/// Payloads are cut here before they are stored (a tool dumping a log must not fill the disk).
pub const PAYLOAD_MAX: usize = 1 << 20;

/// `(open turn id, last turn id)` of a run.
type Cursor = (Option<String>, Option<String>);

#[derive(Default)]
pub struct State {
    /// run id -> its turn cursor. Validated against the store on use.
    cur: Mutex<HashMap<String, Cursor>>,
}

pub struct Payload {
    pub data: Vec<u8>,
    pub ext: &'static str,
    pub mime: &'static str,
}

pub struct ItemSpec {
    pub kind: ItemKind,
    pub summary: String,
    pub native_id: Option<String>,
    pub payload: Option<Payload>,
    pub file_change: Option<FileChange>,
    /// The item is complete when recorded (messages, results); tool calls finish later.
    pub finished: bool,
}

impl ItemSpec {
    pub fn new(kind: ItemKind, summary: &str) -> Self {
        ItemSpec {
            kind,
            summary: summarize(summary, SUMMARY_MAX),
            native_id: None,
            payload: None,
            file_change: None,
            finished: true,
        }
    }
}

/// Redacted, whitespace-collapsed, bounded one-line summary.
pub fn summarize(text: &str, max: usize) -> String {
    let red = vk_redact::redact(text);
    let one: String = red.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= max {
        one
    } else {
        let mut t: String = one.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// A payload for `text` when it is longer than [`INLINE_MAX`] (redacted, cut at
/// [`PAYLOAD_MAX`]).
pub fn text_payload(text: &str) -> Option<Payload> {
    if text.len() <= INLINE_MAX {
        return None;
    }
    let red = vk_redact::redact(text);
    let mut end = red.len().min(PAYLOAD_MAX);
    while !red.is_char_boundary(end) {
        end -= 1;
    }
    Some(Payload {
        data: red.as_bytes()[..end].to_vec(),
        ext: "txt",
        mime: "text/plain",
    })
}

fn usage_of(u: &RunUsage) -> TurnUsage {
    TurnUsage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read: u.cache_read_tokens,
        cache_write: u.cache_write_tokens,
        cost_usd: u.cost_usd,
    }
}

fn delta(now: &TurnUsage, base: &TurnUsage) -> TurnUsage {
    TurnUsage {
        input_tokens: now.input_tokens.saturating_sub(base.input_tokens),
        output_tokens: now.output_tokens.saturating_sub(base.output_tokens),
        cache_read: now.cache_read.saturating_sub(base.cache_read),
        cache_write: now.cache_write.saturating_sub(base.cache_write),
        cost_usd: match (now.cost_usd, base.cost_usd) {
            (Some(a), Some(b)) => Some((a - b).max(0.0)),
            (Some(a), None) => Some(a),
            _ => None,
        },
    }
}

fn turns_of(core: &Core, run: &str) -> Vec<Turn> {
    core.store
        .load_by_field::<Turn>(K_TURN, "$.run_id", run)
        .unwrap_or_default()
}

/// The run's open turn, if any (cache first, store otherwise).
fn open_turn(server: &Server, core: &Core, run: &str) -> Option<Turn> {
    let cached = server
        .items
        .cur
        .lock()
        .unwrap()
        .get(run)
        .and_then(|c| c.0.clone());
    if let Some(id) = cached
        && let Ok(Some(t)) = core.store.get::<Turn>(K_TURN, &id)
        && t.status == TurnStatus::Running
    {
        return Some(t);
    }
    turns_of(core, run)
        .into_iter()
        .rfind(|t| t.status == TurnStatus::Running)
}

fn remember(server: &Server, run: &str, open: Option<&str>, last: &str) {
    server.items.cur.lock().unwrap().insert(
        run.to_string(),
        (open.map(str::to_string), Some(last.to_string())),
    );
}

fn subject(run: &AgentRun) -> Value {
    json!({"run": run.id, "pane": run.pane})
}

fn new_turn(core: &Core, run: &AgentRun, summary: &str) -> Turn {
    Turn {
        id: crate::core::ulid(),
        run_id: run.id.clone(),
        seq: turns_of(core, &run.id).len() as u32 + 1,
        started_at_ms: now_ms(),
        ended_at_ms: None,
        input_summary: summarize(summary, SUMMARY_MAX),
        status: TurnStatus::Running,
        usage: None,
        usage_baseline: (!run.usage.source.is_empty()).then(|| usage_of(&run.usage)),
        item_count: 0,
    }
}

/// A prompt was submitted: a new turn starts. A turn still open for the run is closed as
/// interrupted first. Returns the turn id.
pub fn turn_started(server: &Server, run: &AgentRun, summary: &str) -> Option<String> {
    let mut c = server.core.lock().unwrap();
    // Usage totals of the live run, not the snapshot the caller held.
    let live = c.run(&run.id).cloned().unwrap_or_else(|| run.clone());
    let mut tx = Tx::new();
    if let Some(mut old) = open_turn(server, &c, &run.id) {
        old.status = TurnStatus::Interrupted;
        old.ended_at_ms = Some(now_ms());
        tx.m.put(K_TURN, &old.id, None, &old);
    }
    let t = new_turn(&c, &live, summary);
    tx.m.put(K_TURN, &t.id, None, &t);
    let id = t.id.clone();
    server.commit(&mut c, tx).ok()?;
    remember(server, &run.id, Some(&id), &id);
    Some(id)
}

/// Record an item in the run's open turn (an implicit turn is opened when none is, for runs
/// adopted mid-turn). Returns the item id.
pub fn item(server: &Server, run: &AgentRun, spec: ItemSpec) -> Option<String> {
    // The blob goes in before the core lock is taken: `put_payload` reads the model.
    let payload_ref = spec.payload.as_ref().and_then(|p| {
        crate::blob_store::put_payload(server, &p.data, p.ext, p.mime, Some(&run.pane)).ok()
    });
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    let mut turn = match open_turn(server, &c, &run.id) {
        Some(t) => t,
        None => new_turn(&c, run, "(turn began before Vibeke saw it)"),
    };
    turn.item_count += 1;
    let now = now_ms();
    let it = Item {
        id: crate::core::ulid(),
        turn_id: turn.id.clone(),
        run_id: run.id.clone(),
        seq: turn.item_count,
        kind: spec.kind,
        started_at_ms: now,
        ended_at_ms: spec.finished.then_some(now),
        summary: spec.summary,
        payload_ref,
        file_change: spec.file_change,
        native_id: spec.native_id,
    };
    tx.event(
        "agent.item",
        subject(run),
        json!({
            "kind": it.kind.as_str(),
            "summary": it.summary,
            "item": it.id,
            "turn": it.turn_id,
            "seq": it.seq,
            "payload_ref": it.payload_ref,
        }),
    );
    tx.m.put(K_ITEM, &it.id, None, &it);
    tx.m.put(K_TURN, &turn.id, None, &turn);
    let (tid, iid) = (turn.id.clone(), it.id.clone());
    server.commit(&mut c, tx).ok()?;
    remember(server, &run.id, Some(&tid), &tid);
    Some(iid)
}

/// A call item (by the harness's own id) is complete: stamp its end time, optionally replacing
/// its summary.
pub fn item_finished(server: &Server, run: &str, native_id: &str, summary: Option<&str>) -> bool {
    let mut c = server.core.lock().unwrap();
    let found = c
        .store
        .load_by_field::<Item>(K_ITEM, "$.native_id", native_id)
        .unwrap_or_default()
        .into_iter()
        .rfind(|i| i.run_id == run && i.ended_at_ms.is_none());
    let Some(mut it) = found else { return false };
    it.ended_at_ms = Some(now_ms());
    if let Some(s) = summary {
        it.summary = summarize(s, SUMMARY_MAX);
    }
    let mut tx = Tx::new();
    tx.m.put(K_ITEM, &it.id, None, &it);
    server.commit(&mut c, tx).is_ok()
}

/// The turn ended.
pub fn turn_ended(server: &Server, run: &str, status: TurnStatus) {
    let mut c = server.core.lock().unwrap();
    let Some(mut t) = open_turn(server, &c, run) else {
        return;
    };
    t.status = status;
    t.ended_at_ms = Some(now_ms());
    let mut tx = Tx::new();
    tx.m.put(K_TURN, &t.id, None, &t);
    let id = t.id.clone();
    if server.commit(&mut c, tx).is_ok() {
        remember(server, run, None, &id);
    }
}

/// Session usage totals changed: the latest turn's usage is the delta since it began (the
/// transcript is parsed after `Stop`, so the update usually lands on the turn just ended).
pub fn usage_updated(server: &Server, run: &str, u: &RunUsage) {
    let mut c = server.core.lock().unwrap();
    let last = server
        .items
        .cur
        .lock()
        .unwrap()
        .get(run)
        .and_then(|c| c.1.clone());
    let turn = last
        .and_then(|id| c.store.get::<Turn>(K_TURN, &id).ok().flatten())
        .or_else(|| turns_of(&c, run).pop());
    let Some(mut t) = turn else { return };
    let Some(base) = t.usage_baseline.clone() else {
        return;
    };
    let d = delta(&usage_of(u), &base);
    if t.usage.as_ref() == Some(&d) {
        return;
    }
    t.usage = Some(d);
    let mut tx = Tx::new();
    tx.m.put(K_TURN, &t.id, None, &t);
    let _ = server.commit(&mut c, tx);
}

/// A run ended: its open turn is closed as interrupted, in the same transaction.
pub fn end_run_tx(core: &mut Core, tx: &mut Tx, run: &str) {
    for mut t in turns_of(core, run) {
        if t.status == TurnStatus::Running {
            t.status = TurnStatus::Interrupted;
            t.ended_at_ms = Some(now_ms());
            tx.m.put(K_TURN, &t.id, None, &t);
        }
    }
}

fn emit(server: &Server, run: &AgentRun, kind: &str, data: Value) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(kind, subject(run), data);
    let _ = server.commit(&mut c, tx);
}

fn is_command_tool(tool: &str) -> bool {
    matches!(tool, "Bash" | "shell" | "exec_command" | "local_shell")
}

fn count_lines(s: &str) -> u32 {
    s.lines().count().min(u32::MAX as usize) as u32
}

fn file_change_of(tool: &str, input: &Value) -> Option<FileChange> {
    if !matches!(tool, "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
        return None;
    }
    let path = input.get("file_path").and_then(Value::as_str)?.to_string();
    let text = |k: &str| input.get(k).and_then(Value::as_str);
    let (op, added, removed) = match tool {
        "Write" => (FileOp::Create, text("content").map(count_lines), None),
        "Edit" => (
            FileOp::Modify,
            text("new_string").map(count_lines),
            text("old_string").map(count_lines),
        ),
        _ => (FileOp::Modify, None, None),
    };
    Some(FileChange {
        path,
        op,
        lines_added: added,
        lines_removed: removed,
    })
}

fn stringify(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Map a Claude/Codex hook event onto the stream (`on_signal`). Cheap for events it ignores.
pub fn observe_hook(server: &Server, run: &AgentRun, event: &str, p: &Value) {
    match event {
        "UserPromptSubmit" => {
            let prompt = p.get("prompt").and_then(Value::as_str).unwrap_or("");
            turn_started(server, run, prompt);
            let mut spec = ItemSpec::new(ItemKind::UserMessage, prompt);
            spec.payload = text_payload(prompt);
            item(server, run, spec);
        }
        "PreToolUse" => {
            let tool = p.get("tool_name").and_then(Value::as_str).unwrap_or("");
            let input = p.get("tool_input").cloned().unwrap_or(Value::Null);
            let kind = if is_command_tool(tool) {
                ItemKind::Command
            } else {
                ItemKind::ToolCall
            };
            let mut spec = ItemSpec::new(kind, &crate::agents::harness::tool_summary(tool, &input));
            spec.native_id = p
                .get("tool_use_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            spec.finished = false;
            item(server, run, spec);
        }
        "PostToolUse" | "PostToolUseFailure" => {
            let tool = p.get("tool_name").and_then(Value::as_str).unwrap_or("");
            let input = p.get("tool_input").cloned().unwrap_or(Value::Null);
            let failed = event == "PostToolUseFailure";
            let native = p.get("tool_use_id").and_then(Value::as_str);
            if let Some(n) = native {
                item_finished(server, &run.id, n, None);
            }
            let out = stringify(
                p.get("tool_response")
                    .or_else(|| p.get("error"))
                    .unwrap_or(&Value::Null),
            );
            let head = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            let mut spec = ItemSpec::new(
                if failed {
                    ItemKind::Error
                } else {
                    ItemKind::ToolResult
                },
                &format!("{tool} {}: {head}", if failed { "failed" } else { "ok" }),
            );
            spec.payload = text_payload(&out);
            item(server, run, spec);
            if !failed && let Some(fc) = file_change_of(tool, &input) {
                let mut spec = ItemSpec::new(
                    ItemKind::FileChange,
                    &format!("{:?} {}", fc.op, fc.path).to_lowercase(),
                );
                spec.file_change = Some(fc);
                item(server, run, spec);
            }
        }
        "Stop" | "Interrupt" => {
            if let Some(m) = p
                .get("last_assistant_message")
                .and_then(Value::as_str)
                .filter(|m| !m.trim().is_empty())
            {
                let mut spec = ItemSpec::new(ItemKind::AssistantMessage, m);
                spec.payload = text_payload(m);
                item(server, run, spec);
            }
            turn_ended(
                server,
                &run.id,
                if event == "Interrupt" {
                    TurnStatus::Interrupted
                } else {
                    TurnStatus::Completed
                },
            );
        }
        "StopFailure" => {
            let kind = p
                .get("error_type")
                .or_else(|| p.get("matcher"))
                .or_else(|| p.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("error");
            let msg = p.get("message").and_then(Value::as_str).unwrap_or("");
            item(
                server,
                run,
                ItemSpec::new(ItemKind::Error, &format!("{kind}: {msg}")),
            );
            turn_ended(server, &run.id, TurnStatus::Failed);
        }
        "SubagentStart" => {
            let id = p.get("agent_id").and_then(Value::as_str);
            let ty = p.get("agent_type").and_then(Value::as_str).unwrap_or("");
            let mut spec = ItemSpec::new(ItemKind::Subagent, &format!("subagent {ty}"));
            spec.native_id = id.map(|i| format!("subagent:{i}"));
            spec.finished = false;
            item(server, run, spec);
            emit(
                server,
                run,
                "agent.subagent_started",
                json!({"agent_id": id, "agent_type": ty}),
            );
        }
        "SubagentStop" => {
            let id = p.get("agent_id").and_then(Value::as_str);
            let ty = p.get("agent_type").and_then(Value::as_str).unwrap_or("");
            if let Some(i) = id {
                item_finished(server, &run.id, &format!("subagent:{i}"), None);
            }
            emit(
                server,
                run,
                "agent.subagent_finished",
                json!({"agent_id": id, "agent_type": ty}),
            );
        }
        _ => {}
    }
}

/// Blob hashes referenced by item payloads (what `blob.gc` must keep).
pub fn payload_refs(server: &Server) -> HashSet<String> {
    server.with_core(|c| {
        c.store
            .load::<Item>(K_ITEM)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|i| i.payload_ref)
            .collect()
    })
}

/// Remove turns that ended before `cutoff_ms` together with their items. Returns the number of
/// rows (turns and items) removed.
pub fn prune(server: &Server, cutoff_ms: i64) -> usize {
    let mut c = server.core.lock().unwrap();
    let old: Vec<Turn> = c
        .store
        .load::<Turn>(K_TURN)
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.ended_at_ms.is_some_and(|e| e < cutoff_ms))
        .collect();
    let mut removed = 0;
    for chunk in old.chunks(200) {
        let mut tx = Tx::new();
        for t in chunk {
            for it in c
                .store
                .load_by_field::<Item>(K_ITEM, "$.turn_id", &t.id)
                .unwrap_or_default()
            {
                tx.m.delete(K_ITEM, &it.id);
                removed += 1;
            }
            tx.m.delete(K_TURN, &t.id);
            removed += 1;
        }
        if server.commit(&mut c, tx).is_err() {
            break;
        }
    }
    removed
}

/// `vibeke forget` (09 §9.3, lane 3E `forget_scope`): remove the turns (and their items) for
/// which `covers(run_id, started_at_ms)` holds, plus items of those runs without a turn.
/// `dry_run` only counts. Returns the rows (turns and items) removed.
pub fn forget(server: &Server, covers: &dyn Fn(&str, i64) -> bool, dry_run: bool) -> usize {
    let mut c = server.core.lock().unwrap();
    let turns: Vec<Turn> = c.store.load::<Turn>(K_TURN).unwrap_or_default();
    let items: Vec<Item> = c.store.load::<Item>(K_ITEM).unwrap_or_default();
    let gone_turns: HashSet<String> = turns
        .iter()
        .filter(|t| covers(&t.run_id, t.started_at_ms))
        .map(|t| t.id.clone())
        .collect();
    let gone_items: Vec<&Item> = items
        .iter()
        .filter(|i| gone_turns.contains(&i.turn_id) || covers(&i.run_id, i.started_at_ms))
        .collect();
    let n = gone_turns.len() + gone_items.len();
    if dry_run || n == 0 {
        return n;
    }
    let mut tx = Tx::new();
    for t in &gone_turns {
        tx.m.delete(K_TURN, t);
    }
    for i in &gone_items {
        tx.m.delete(K_ITEM, &i.id);
    }
    if server.commit(&mut c, tx).is_err() {
        return 0;
    }
    n
}

fn resolve_run(server: &Server, t: &str) -> String {
    server
        .with_core(|c| c.run(t).map(|r| r.id.clone()))
        .unwrap_or_else(|| t.to_string())
}

fn limit_of(p: &Value, default: usize) -> usize {
    u(p, "limit")
        .map(|l| l as usize)
        .unwrap_or(default)
        .clamp(1, 1000)
}

fn turns_api(server: &Server, p: &Value) -> R {
    let t = crate::api::req(p, "run")?;
    let run = resolve_run(server, t);
    let after = u(p, "after_seq").unwrap_or(0) as u32;
    let limit = limit_of(p, 100);
    let mut turns: Vec<Turn> = server.with_core(|c| turns_of(c, &run));
    if turns.is_empty() {
        return Err(not_found("run", t));
    }
    turns.sort_by_key(|t| t.seq);
    let turns: Vec<Turn> = turns.into_iter().filter(|t| t.seq > after).collect();
    let more = turns.len() > limit;
    let turns: Vec<Turn> = turns.into_iter().take(limit).collect();
    Ok(
        json!({"run": run, "turns": turns, "next_after_seq": more.then(|| turns.last().map(|t| t.seq)).flatten()}),
    )
}

fn items_api(server: &Server, p: &Value) -> R {
    let limit = limit_of(p, 200);
    let after = u(p, "after_seq").unwrap_or(0) as u32;
    let kind = match s(p, "kind") {
        None => None,
        Some(k) => {
            Some(ItemKind::parse(k).ok_or_else(|| invalid(format!("unknown item kind {k}")))?)
        }
    };
    let mut items: Vec<Item> = match (s(p, "turn"), s(p, "run")) {
        (Some(t), _) => server.with_core(|c| {
            c.store
                .load_by_field::<Item>(K_ITEM, "$.turn_id", t)
                .unwrap_or_default()
        }),
        (None, Some(r)) => {
            let run = resolve_run(server, r);
            server.with_core(|c| {
                c.store
                    .load_by_field::<Item>(K_ITEM, "$.run_id", &run)
                    .unwrap_or_default()
            })
        }
        _ => return Err(invalid("turn or run required")),
    };
    // Seq is per turn; a run listing is in recording order, so `after_seq` only narrows a
    // single turn.
    if s(p, "turn").is_some() {
        items.sort_by_key(|i| i.seq);
        items.retain(|i| i.seq > after);
    } else if after > 0 {
        return Err(err(
            ErrorKind::InvalidParams,
            "after_seq applies to a single turn; page a run with turn",
        ));
    }
    if let Some(k) = kind {
        items.retain(|i| i.kind == k);
    }
    let more = items.len() > limit;
    items.truncate(limit);
    let next = more.then(|| items.last().map(|i| i.seq)).flatten();
    Ok(json!({"items": items, "next_after_seq": next}))
}

/// Dispatch hook for `agent.turns` / `agent.items`.
pub fn api(server: &Server, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "agent.turns" => turns_api(server, p),
        "agent.items" => items_api(server, p),
        _ => return None,
    })
}

/// Server start hook: nothing to rebuild (the cache fills from the store on first use); kept so
/// the start sequence reads the same as the other modules.
pub fn start(_server: &std::sync::Arc<Server>) {}

#[cfg(test)]
#[path = "items_tests.rs"]
mod tests;
