//! Server pieces for the phone gateway (spec 16 §13 X1–X5): styled pane mirror, structured
//! transcripts with stable paging, client activity/focus, gateway identity + actor audit, and
//! out-of-band confirmations shown by the TUI.

use crate::Server;
use crate::api::{Ctx, R, err, invalid, req, s, u};
use crate::core::Tx;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use vk_proto::render::{Color, Row, attr};
use vk_proto::rpc::ErrorKind;

#[derive(Default)]
pub struct State {
    /// Host-wide last input per TUI client (ms since epoch), reported by `client.activity`.
    activity: Mutex<HashMap<String, i64>>,
    confirms: Mutex<HashMap<String, oneshot::Sender<Option<String>>>>,
    /// Devices a gateway reports as connected (`client.devices`), by reporting client id.
    devices: Mutex<HashMap<String, Vec<Value>>>,
    /// Requests waiting for the gateway, and the gateways connected (`gateway.call`).
    pub(crate) bridge: crate::gateway_bridge::Bridge,
    /// The supervised gateway process (`gateway.start` / `gateway.stop`).
    pub(crate) supervisor: crate::gateway_supervisor::State,
}

fn state(server: &Server) -> &State {
    &server.gateway
}

/// Runs before normal dispatch. `None` lets the regular handler run.
pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if matches!(ctx.kind.as_str(), "gateway" | "gateway-tui")
        && let Some(r) = audit(server, ctx, method, p)
    {
        return Some(r);
    }
    Some(match method {
        "pane.read" if s(p, "source") == Some("styled") => styled(server, ctx, p),
        "agent.transcript"
            if p.get("before").is_some()
                || p.get("items").and_then(Value::as_bool) == Some(true)
                || p.get("image").is_some()
                || matches!(ctx.kind.as_str(), "gateway" | "gateway-tui") =>
        {
            transcript(server, ctx, p)
        }
        "client.list" => Ok(client_list(server)),
        "client.activity" => {
            let at = p
                .get("last_input_ms")
                .and_then(Value::as_i64)
                .unwrap_or_else(vk_store::now_ms);
            state(server)
                .activity
                .lock()
                .unwrap()
                .insert(ctx.client_id.clone(), at);
            Ok(json!({}))
        }
        "client.devices" => {
            // A gateway reports the phones/desktops connected through it (replaces its list).
            if ctx.kind != "gateway" {
                return Some(Err(err(
                    ErrorKind::PermissionDenied,
                    "client.devices is reported by gateway clients",
                )));
            }
            let list: Vec<Value> = p
                .get("devices")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .take(64)
                .collect();
            let n = list.len();
            let prev = state(server)
                .devices
                .lock()
                .unwrap()
                .insert(ctx.client_id.clone(), list.clone());
            if prev.as_ref() != Some(&list) {
                // Event push: TUIs refresh their devices indicator without polling.
                let mut c = server.core.lock().unwrap();
                let mut tx = Tx::new();
                tx.event(
                    "client.devices_changed",
                    json!({"client": ctx.client_id}),
                    json!({"devices": n}),
                );
                let _ = server.commit(&mut c, tx);
            }
            Ok(json!({}))
        }
        "client.confirm" => confirm(server, ctx, p).await,
        "client.confirm_answer" => confirm_answer(server, ctx, p),
        _ => return None,
    })
}

fn mutating(method: &str) -> bool {
    crate::api::METHODS
        .iter()
        .chain(crate::agents::METHODS.iter())
        .chain(crate::desk::METHODS.iter())
        .chain(crate::drafts::METHODS.iter())
        .chain(crate::handoff::METHODS.iter())
        .find(|(n, _)| *n == method)
        .map(|(_, m)| *m)
        .unwrap_or(
            method.contains(".answer") || method.contains(".send") || method.contains(".prompt"),
        )
}

/// X4: a gateway acts for a device; every mutation names it, and the audit trail records it.
fn audit(server: &Server, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if !mutating(method) {
        return None;
    }
    let Some(actor) = s(p, "actor").filter(|a| !a.trim().is_empty()) else {
        return Some(Err(invalid(format!(
            "{method}: gateway clients must pass `actor` (e.g. \"gateway:<device>\")"
        ))));
    };
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event_by(
        "client.action",
        json!({"method": method, "pane": s(p, "pane"), "target": s(p, "target")}),
        json!({"kind": "gateway", "id": actor, "client": ctx.client_id}),
        json!({}),
    );
    let _ = server.commit(&mut c, tx);
    None
}

// ---- X1: styled pane rows -------------------------------------------------------------------

fn color(c: &Color) -> Value {
    match c {
        Color::Default => Value::Null,
        Color::Indexed(i) => json!(i),
        Color::Rgb(r, g, b) => json!(format!("#{r:02x}{g:02x}{b:02x}")),
    }
}

pub fn styled_rows(rows: &[Row]) -> Vec<Value> {
    rows.iter()
        .map(|r| {
            let mut col = 0u32;
            let runs: Vec<Value> = r
                .spans
                .iter()
                .filter_map(|sp| {
                    let start = col;
                    col += sp.cols as u32;
                    let st = &sp.style;
                    let plain = st.fg == Color::Default && st.bg == Color::Default && st.attrs == 0;
                    (!plain).then(|| {
                        json!({
                            "start": start, "len": sp.cols,
                            "fg": color(&st.fg), "bg": color(&st.bg),
                            "bold": st.attrs & attr::BOLD != 0,
                            "dim": st.attrs & attr::DIM != 0,
                            "italic": st.attrs & attr::ITALIC != 0,
                            "underline": st.attrs & attr::ANY_UNDERLINE != 0,
                            "inverse": st.attrs & attr::INVERSE != 0,
                        })
                    })
                })
                .collect();
            json!({"text": r.text(), "wrapped": r.wrapped, "runs": runs})
        })
        .collect()
}

fn styled(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let pane = crate::api::resolve_pane(server, ctx, s(p, "pane"))?;
    let rt = server
        .pane_rt(&pane.id)
        .ok_or_else(|| err(ErrorKind::NotFound, "pane has no live screen"))?;
    let sc = rt.screen.lock().unwrap();
    let e = &sc.engine;
    let lines = u(p, "lines").unwrap_or(e.rows() as u64) as usize;
    let mut rows: Vec<Row> = Vec::new();
    let h = e.history_len();
    let want_hist = lines.saturating_sub(e.rows() as usize).min(h);
    for i in h - want_hist..h {
        if let Some(r) = e.history_row(i) {
            rows.push(r);
        }
    }
    rows.extend(e.visible_rows());
    let n = rows.len();
    let rows = &rows[n.saturating_sub(lines)..];
    let cur = e.cursor();
    Ok(
        json!({"pane": pane.id, "cols": e.cols(), "rows": styled_rows(rows), "cursor": serde_json::to_value(cur).unwrap_or(Value::Null)}),
    )
}

// ---- X2: structured transcript --------------------------------------------------------------

fn summarize(v: &Value) -> String {
    let t = match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let one: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(160).collect()
}

/// Keep expandable tool details bounded without flattening their line breaks.
fn transcript_detail(text: &str) -> String {
    const LIMIT: usize = 8192;
    let mut chars = text.chars();
    let mut detail: String = chars.by_ref().take(LIMIT).collect();
    if chars.next().is_some() {
        detail.push_str("\n…");
    }
    detail
}

fn execution_envelope(v: &Value) -> bool {
    v.get("output").is_some()
        && (v.get("chunk_id").is_some()
            || v.get("exit_code").is_some()
            || v["wall_time_seconds"].is_number()
            || v["metadata"].get("exit_code").is_some())
}

fn transcript_exit_code(v: &Value, depth: usize) -> Option<i64> {
    if depth >= 4 {
        return None;
    }
    if let Some(s) = v.as_str() {
        return serde_json::from_str::<Value>(s)
            .ok()
            .and_then(|v| transcript_exit_code(&v, depth + 1));
    }
    if let Some(a) = v.as_array() {
        let codes: Vec<i64> = a
            .iter()
            .filter_map(|b| b.get("text"))
            .filter_map(|b| transcript_exit_code(b, depth + 1))
            .collect();
        return codes
            .iter()
            .copied()
            .find(|c| *c != 0)
            .or_else(|| codes.first().copied());
    }
    if execution_envelope(v) {
        return v["exit_code"]
            .as_i64()
            .or_else(|| v.pointer("/metadata/exit_code").and_then(Value::as_i64));
    }
    None
}

/// Text blocks and execution envelopes are transport wrappers, not the command's output.
fn transcript_output(v: &Value, depth: usize) -> String {
    if depth < 4 {
        if let Some(s) = v.as_str()
            && let Ok(parsed) = serde_json::from_str::<Value>(s)
            && (parsed.is_array() || execution_envelope(&parsed))
        {
            return transcript_output(&parsed, depth + 1);
        }
        if let Some(a) = v.as_array() {
            let texts: Vec<String> = a
                .iter()
                .filter(|b| {
                    matches!(
                        b["type"].as_str(),
                        Some("text" | "input_text" | "output_text" | "summary_text")
                    )
                })
                .filter_map(|b| b.get("text"))
                .map(|t| transcript_output(t, depth + 1))
                .collect();
            if a.is_empty()
                || !texts.is_empty()
                || a.iter()
                    .any(|b| matches!(b["type"].as_str(), Some("image" | "input_image")))
            {
                return texts.join("\n");
            }
        }
        if execution_envelope(v) {
            // Once the execution wrapper is removed, stdout is opaque. A program may print
            // JSON with fields such as `output` and `exit_code` as its own domain data.
            if let Some(text) = v["output"].as_str() {
                return text.to_string();
            }
            return transcript_output(&v["output"], depth + 1);
        }
    }
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn transcript_result(output: &Value, id: &Value, error: bool) -> Value {
    let code = transcript_exit_code(output, 0);
    let error = error || code.is_some_and(|c| c != 0);
    let mut text = transcript_detail(&transcript_output(output, 0));
    if let Some(code) = code.filter(|c| *c != 0) {
        text.push_str(&format!("\nCommand exited with code {code}."));
    }
    json!({"kind": "tool_result", "summary": summarize(&json!(text)), "text": text, "id": id, "error": error})
}

fn transcript_call(tool: &Value, input: &Value, id: &Value) -> Value {
    let text = transcript_detail(input.as_str().unwrap_or(&input.to_string()));
    json!({"kind": "tool_call", "tool": tool, "summary": summarize(&json!(text)), "text": text, "id": id})
}

/// Largest transcript image `agent.transcript` returns inline (decoded bytes).
const MAX_TRANSCRIPT_IMAGE: usize = 4 << 20;

/// An inline image block as `(mime, base64 data)`. Claude and pi store base64 data in the block
/// (`source.data` + `source.media_type`, or `data` + `mimeType`); Codex stores a `data:` URL.
/// Only png, jpeg, gif and webp count, and a block that links to a URL has no data here.
fn image_of(c: &Value) -> Option<(String, &str)> {
    let (mime, data) = match c.get("type").and_then(Value::as_str)? {
        "image" => {
            let src = c.get("source").unwrap_or(c);
            let mime = ["media_type", "mimeType", "mime_type"]
                .iter()
                .find_map(|k| src.get(*k).and_then(Value::as_str))?;
            (mime.to_string(), src.get("data").and_then(Value::as_str)?)
        }
        "input_image" | "output_image" => {
            let url = c.get("image_url")?;
            let url = url
                .as_str()
                .or_else(|| url.get("url").and_then(Value::as_str))?;
            let (head, data) = url.strip_prefix("data:")?.split_once(',')?;
            (head.strip_suffix(";base64")?.to_string(), data)
        }
        _ => return None,
    };
    let mime = mime.to_ascii_lowercase();
    let known = matches!(
        mime.as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    );
    (known && !data.is_empty()).then_some((mime, data))
}

/// The content blocks of a line that can hold images, in document order: Claude and pi message
/// content (including the content of a tool result) and Codex message content.
fn image_candidates(v: &Value) -> Vec<&Value> {
    let blocks = v
        .get("message")
        .and_then(|m| m.get("content"))
        .or_else(|| {
            (v.get("type").and_then(Value::as_str) == Some("response_item"))
                .then(|| v.get("payload")?.get("content"))
                .flatten()
        })
        .and_then(Value::as_array);
    let mut out = Vec::new();
    for c in blocks.into_iter().flatten() {
        out.push(c);
        if let Some(inner) = c.get("content").and_then(Value::as_array) {
            out.extend(inner);
        }
    }
    out
}

/// The images of one transcript line, in the order `line_items` numbers them (`ref` k).
fn line_images(v: &Value) -> Vec<(String, &str)> {
    image_candidates(v)
        .into_iter()
        .filter_map(image_of)
        .collect()
}

fn image_item(k: usize, mime: &str, data: &str) -> Value {
    // `ref` is completed with the line number by `transcript_items`.
    json!({"kind": "image", "mime": mime, "ref": k.to_string(), "size": data.len() / 4 * 3})
}

/// The `bg_event` item of a Claude line that is a task notification (background work ended, or
/// a Monitor reported an event): `{kind, id, tool_use_id, status, summary}`.
fn bg_event_item(v: &Value) -> Option<Value> {
    if v.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let text = match v.pointer("/message/content")? {
        Value::String(t) => t.clone(),
        Value::Array(a) => a
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let n = vk_agents::transcript::parse_task_notification(&text)?;
    let red = vk_redact::redact(&n.summary);
    let mut summary: String = red.chars().take(300).collect();
    if red.chars().count() > 300 {
        summary.push('…');
    }
    Some(json!({
        "kind": "bg_event",
        "id": n.task_id,
        "tool_use_id": n.tool_use_id,
        "status": n.status.unwrap_or_else(|| "event".into()),
        "summary": summary,
    }))
}

/// Items from one transcript line, for Claude (`message.content[]`) and Codex rollout
/// (`response_item` payloads). Returns (is_user_prompt, items).
pub(crate) fn line_items(v: &Value) -> (bool, Vec<Value>) {
    let mut items = Vec::new();
    let mut user_prompt = false;
    if let Some(item) = bg_event_item(v) {
        // The agent wakes up for it: it starts a turn, like a prompt.
        return (true, vec![item]);
    }
    if let Some(msg) = v.get("message") {
        let role = v.get("type").and_then(Value::as_str).unwrap_or("");
        match msg.get("content") {
            Some(Value::String(t)) => {
                user_prompt = role == "user";
                items.push(json!({"kind": "text", "role": role, "text": t}));
            }
            Some(Value::Array(a)) => {
                let mut images = 0usize;
                for c in a {
                    match c.get("type").and_then(Value::as_str) {
                        Some("image") => {
                            if let Some((mime, data)) = image_of(c) {
                                items.push(image_item(images, &mime, data));
                                images += 1;
                            }
                        }
                        Some("text") => {
                            user_prompt |= role == "user";
                            items
                                .push(json!({"kind": "text", "role": role, "text": c.get("text")}));
                        }
                        Some("thinking") => {
                            items.push(json!({"kind": "thinking", "text": c.get("thinking")}))
                        }
                        Some("tool_use") => {
                            items.push(transcript_call(&c["name"], &c["input"], &c["id"]))
                        }
                        Some("tool_result") => {
                            items.push(transcript_result(
                                &c["content"],
                                &c["tool_use_id"],
                                c["is_error"].as_bool().unwrap_or(false),
                            ));
                            for inner in c
                                .get("content")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                            {
                                if let Some((mime, data)) = image_of(inner) {
                                    items.push(image_item(images, &mime, data));
                                    images += 1;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    } else if v.get("type").and_then(Value::as_str) == Some("response_item") {
        let pl = v.get("payload").unwrap_or(&Value::Null);
        match pl.get("type").and_then(Value::as_str) {
            Some("message") => {
                let role = pl.get("role").and_then(Value::as_str).unwrap_or("");
                let text: String = pl
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| c.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if !text.is_empty() {
                    user_prompt = role == "user";
                    items.push(json!({"kind": "text", "role": role, "text": text}));
                }
                for (k, (mime, data)) in line_images(v).into_iter().enumerate() {
                    items.push(image_item(k, &mime, data));
                }
            }
            Some("reasoning") => {
                let text = transcript_output(&pl["summary"], 0);
                if !text.trim().is_empty() {
                    items.push(json!({"kind": "thinking", "text": transcript_detail(&text)}));
                }
            }
            Some("function_call") | Some("local_shell_call") | Some("custom_tool_call") => items
                .push(transcript_call(
                    &pl["name"],
                    pl.get("arguments")
                        .or(pl.get("input"))
                        .or(pl.get("action"))
                        .unwrap_or(&Value::Null),
                    &pl["call_id"],
                )),
            Some("function_call_output") | Some("custom_tool_call_output") => {
                items.push(transcript_result(&pl["output"], &pl["call_id"], false))
            }
            _ => {}
        }
    }
    (user_prompt, items)
}

/// A transcript line's timestamp in epoch ms: an RFC 3339 string (both harness formats) or a
/// number (seconds or ms).
fn line_ts_ms(v: &Value) -> Option<i64> {
    match v.get("timestamp")? {
        Value::String(t) => crate::agents::usage::parse_rfc3339_ms(t),
        Value::Number(n) => n
            .as_i64()
            .map(|n| if n < 100_000_000_000 { n * 1000 } else { n }),
        _ => None,
    }
}

/// Tool names that start a subagent (Claude `Task`/`Agent`, Codex agent spawning).
fn is_subagent_tool(name: &str) -> bool {
    matches!(
        name,
        "Task" | "Agent" | "spawn_agent" | "spawn_subagent" | "create_agent"
    )
}

/// Per-turn timing and counts: `duration_ms` (harness duration, else last item ts − turn start),
/// `tool_count` and `subagent_count`.
fn finish_turn(turn: &mut Value) {
    let items = turn["items"].as_array().cloned().unwrap_or_default();
    let start = turn
        .get("ts")
        .and_then(Value::as_str)
        .and_then(crate::agents::usage::parse_rfc3339_ms)
        .or_else(|| items.first().and_then(|i| i["ts"].as_i64()));
    let end = items.iter().rev().find_map(|i| i["ts"].as_i64());
    let duration = turn["duration_ms"]
        .as_i64()
        .or_else(|| start.zip(end).map(|(a, b)| (b - a).max(0)));
    let calls = items.iter().filter(|i| i["kind"] == "tool_call");
    let tool_count = calls.clone().count();
    let subagents = calls
        .filter(|i| i["tool"].as_str().is_some_and(is_subagent_tool))
        .count();
    turn["duration_ms"] = json!(duration);
    turn["tool_count"] = json!(tool_count);
    turn["subagent_count"] = json!(subagents);
}

/// Turns numbered from the start of the transcript (stable `n` across calls). A turn starts at
/// a user prompt (not a tool result). Items carry `ts` (epoch ms, null when the line has none);
/// turns carry `duration_ms`, `tool_count` and `subagent_count`.
pub fn transcript_items(text: &str) -> Vec<Value> {
    let mut turns: Vec<Value> = Vec::new();
    for (line_no, l) in text.lines().enumerate() {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        // Codex reports the time shown in its terminal, including tool and wait time.
        if v["type"] == "event_msg"
            && matches!(
                v["payload"]["type"].as_str(),
                Some("task_complete" | "turn_complete")
            )
            && let Some(duration) = v["payload"]["duration_ms"].as_u64()
            && let Some(turn) = turns.last_mut()
        {
            turn["duration_ms"] = json!(duration);
        }
        let (prompt, mut items) = line_items(&v);
        if items.is_empty() {
            continue;
        }
        let ts = line_ts_ms(&v);
        for it in &mut items {
            it["ts"] = json!(ts);
            if it["kind"] == "image" {
                // `<line>:<k>`: the transcript line and the image's position in it.
                let k = it["ref"].as_str().unwrap_or("0").to_string();
                it["ref"] = json!(format!("{line_no}:{k}"));
            }
        }
        if prompt || turns.is_empty() {
            turns.push(json!({"n": turns.len() as u64 + 1, "ts": v.get("timestamp"), "items": []}));
        }
        let last = turns.last_mut().unwrap();
        last["items"].as_array_mut().unwrap().extend(items);
    }
    for t in &mut turns {
        finish_turn(t);
    }
    turns
}

/// The image a transcript item's `ref` (`<line>:<k>`) names: `{mime, size, data_b64}`.
fn transcript_image(text: &str, r: &str) -> R {
    let (line, k) = r
        .split_once(':')
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
        .ok_or_else(|| invalid("image ref must be <line>:<index>"))?;
    let v: Value = text
        .lines()
        .nth(line)
        .and_then(|l| serde_json::from_str(l).ok())
        .ok_or_else(|| err(ErrorKind::NotFound, "no such transcript line"))?;
    let (mime, data) = line_images(&v)
        .into_iter()
        .nth(k)
        .ok_or_else(|| err(ErrorKind::NotFound, "no such image"))?;
    let size = data.len() / 4 * 3;
    if size > MAX_TRANSCRIPT_IMAGE {
        return Err(err(ErrorKind::Unsupported, "image larger than 4 MiB"));
    }
    // The data goes into a `data:` URL on the client: only the base64 alphabet may pass.
    if !data
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    {
        return Err(err(ErrorKind::Unsupported, "image data is not base64"));
    }
    Ok(json!({"mime": mime, "size": size, "data_b64": data}))
}

fn transcript(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let t = s(p, "target").or(s(p, "run")).unwrap_or("@current");
    let run = server
        .with_core(|c| c.run(t).cloned())
        .or_else(|| {
            crate::api::resolve_pane(server, ctx, Some(t))
                .ok()
                .and_then(|pane| server.with_core(|c| c.run_for_pane(&pane.id).cloned()))
        })
        .ok_or_else(|| err(ErrorKind::NotFound, format!("no agent run for {t}")))?;
    let path = run.transcript_path.clone().ok_or_else(|| {
        err(ErrorKind::Unsupported, "no transcript for this run")
            .details(json!({"fallback": "agent.read"}))
    })?;
    let meta = std::fs::metadata(&path).map_err(|e| err(ErrorKind::NotFound, e.to_string()))?;
    if meta.len() > 64 << 20 {
        return Err(err(ErrorKind::Unsupported, "transcript larger than 64 MiB"));
    }
    let text =
        std::fs::read_to_string(&path).map_err(|e| err(ErrorKind::Internal, e.to_string()))?;
    if let Some(r) = s(p, "image") {
        let image = transcript_image(&text, r)?;
        return Ok(
            json!({"run": run.id, "session_id": run.harness_session_id, "transcript_path": path, "image": image}),
        );
    }
    let turns = transcript_items(&text);
    let before = u(p, "before").unwrap_or(u64::MAX);
    let limit = u(p, "limit").unwrap_or(20).clamp(1, 200) as usize;
    let page: Vec<Value> = turns
        .into_iter()
        .filter(|t| t["n"].as_u64().unwrap_or(0) < before)
        .collect();
    let n = page.len();
    let page: Vec<Value> = page.into_iter().skip(n.saturating_sub(limit)).collect();
    let next_before = page
        .first()
        .and_then(|t| t["n"].as_u64())
        .filter(|n| *n > 1);
    let mut page = Value::Array(page);
    // Envelopes lose their preview and nonce (spec 19 §4.1).
    crate::mentions::scrub::scrub_value(&mut page);
    Ok(
        json!({"run": run.id, "session_id": run.harness_session_id, "transcript_path": path, "turns": page, "next_before": next_before}),
    )
}

// ---- X3: clients ----------------------------------------------------------------------------

fn client_list(server: &Server) -> Value {
    let activity = state(server).activity.lock().unwrap().clone();
    let now_inst = Instant::now();
    let now_ms = vk_store::now_ms();
    let clients = server.clients.lock().unwrap();
    let list: Vec<Value> = clients
        .iter()
        .map(|(id, c)| {
            let local = c
                .last_active
                .map(|t| now_ms - now_inst.duration_since(t).as_millis() as i64);
            let last = local.into_iter().chain(activity.get(id).copied()).max();
            json!({
                "id": id, "kind": c.kind, "attached_at": c.attached_at_ms,
                "focused_pane": c.focus.pane,
                "focus": {"workspace": c.focus.workspace, "tab": c.focus.tab, "pane": c.focus.pane},
                "host_focused": c.host_focused,
                "last_input_ms": last,
            })
        })
        .collect();
    // Activity reported for TUIs attached to other machines (host-wide "user at desk").
    let elsewhere: Vec<Value> = activity.iter().filter(|(id, _)| !clients.contains_key(*id)).map(|(id, at)| json!({"id": id, "kind": "tui", "remote_activity": true, "last_input_ms": at})).collect();
    // Devices reported by gateways still connected: a gateway's report goes when its connection
    // does (`client_gone`). Gateways talk plain RPC, so they are never in `clients`.
    let dev = state(server).devices.lock().unwrap();
    let devices: Vec<Value> = dev
        .iter()
        .flat_map(|(gw, l)| {
            l.iter().map(move |d| {
                let mut d = d.clone();
                d["via"] = json!(gw);
                d
            })
        })
        .collect();
    drop(dev);
    let user_last = list
        .iter()
        .chain(elsewhere.iter())
        .filter(|c| c["kind"] == "tui")
        .filter_map(|c| c["last_input_ms"].as_i64())
        .max();
    json!({"clients": list, "other_activity": elsewhere, "user_last_input_ms": user_last, "devices": devices})
}

/// A connection ended: the devices its gateway reported are no longer reachable through it.
pub fn client_gone(server: &Server, client_id: &str) {
    let gone = state(server).devices.lock().unwrap().remove(client_id);
    if gone.is_some_and(|l| !l.is_empty()) {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event(
            "client.devices_changed",
            json!({"client": client_id}),
            json!({"devices": 0}),
        );
        let _ = server.commit(&mut c, tx);
    }
}

// ---- X5: out-of-band confirm ----------------------------------------------------------------

async fn confirm(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    if ctx.pane_scope.is_some() {
        return Err(err(
            ErrorKind::PermissionDenied,
            "client.confirm is not available to panes",
        ));
    }
    let title = req(p, "title")?.to_string();
    let options = p
        .get("options")
        .cloned()
        .filter(|o| o.as_array().is_some_and(|a| !a.is_empty()))
        .unwrap_or(json!([{"id": "ok", "label": "OK"}, {"id": "cancel", "label": "Cancel"}]));
    let timeout = Duration::from_millis(u(p, "timeout_ms").unwrap_or(60_000).clamp(1_000, 600_000));
    let tui_attached = server
        .clients
        .lock()
        .unwrap()
        .values()
        .any(|c| c.kind == "tui");
    if !tui_attached {
        return Err(err(
            ErrorKind::Unsupported,
            "no TUI client is attached to show the confirmation",
        ));
    }
    let id = crate::core::ulid();
    let (tx, rx) = oneshot::channel();
    state(server)
        .confirms
        .lock()
        .unwrap()
        .insert(id.clone(), tx);
    {
        let mut c = server.core.lock().unwrap();
        let mut t = Tx::new();
        t.event("client.confirm_requested", json!({"confirm": id}), json!({"title": title, "body": s(p, "body"), "options": options, "timeout_ms": timeout.as_millis() as u64, "from": ctx.client_id}));
        let _ = server.commit(&mut c, t);
    }
    let out = tokio::time::timeout(timeout, rx).await;
    state(server).confirms.lock().unwrap().remove(&id);
    let (choice, timed_out) = match out {
        Ok(Ok(c)) => (c, false),
        _ => (None, true),
    };
    let mut c = server.core.lock().unwrap();
    let mut t = Tx::new();
    t.event(
        "client.confirm_resolved",
        json!({"confirm": id}),
        json!({"choice": choice, "timed_out": timed_out}),
    );
    let _ = server.commit(&mut c, t);
    Ok(json!({"confirm": id, "choice": choice, "timed_out": timed_out}))
}

/// Only a TUI (chrome, out of band of any PTY) may answer a confirmation. A TUI attached over
/// SSH (`render.attach {remote: true}`) is still the user's own terminal; a phone reaches the
/// server only through the gateway (kind `gateway`, never `tui`).
fn may_answer(ctx: &Ctx) -> bool {
    ctx.pane_scope.is_none()
        && match ctx.kind.as_str() {
            "tui" => true,
            "anonymous" => !ctx.remote,
            _ => false,
        }
}

fn confirm_answer(server: &Server, ctx: &Ctx, p: &Value) -> R {
    if !may_answer(ctx) {
        return Err(err(
            ErrorKind::PermissionDenied,
            "confirmations are answered from the TUI",
        ));
    }
    let id = req(p, "confirm")?;
    let tx = state(server).confirms.lock().unwrap().remove(id);
    match tx {
        Some(tx) => {
            let _ = tx.send(s(p, "choice").map(str::to_string));
            Ok(json!({"confirm": id}))
        }
        None => Err(err(
            ErrorKind::NotFound,
            "no pending confirmation with that id",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmations_are_answered_from_a_local_or_ssh_tui_only() {
        let ctx = |kind: &str, remote: bool, pane: Option<&str>| Ctx {
            client_id: "c".into(),
            kind: kind.into(),
            pane_scope: pane.map(str::to_string),
            remote,
        };
        assert!(may_answer(&ctx("tui", false, None)));
        // A TUI on another machine attached through `vibeke bridge`.
        assert!(may_answer(&ctx("tui", true, None)));
        assert!(may_answer(&ctx("anonymous", false, None)));
        assert!(!may_answer(&ctx("anonymous", true, None)));
        assert!(!may_answer(&ctx("gateway", true, None)));
        assert!(!may_answer(&ctx("cli", false, None)));
        assert!(!may_answer(&ctx("tui", false, Some("p1"))));
    }

    #[test]
    fn claude_transcript_turns_are_stable() {
        let t = [
            json!({"type": "user", "message": {"content": "fix it"}, "timestamp": "a"}),
            json!({"type": "assistant", "message": {"content": [{"type": "thinking", "thinking": "hmm"}, {"type": "tool_use", "name": "Bash", "id": "t1", "input": {"command": "cargo test"}}]}}),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "done"}]}}),
            json!({"type": "user", "message": {"content": "next"}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let turns = transcript_items(&t);
        assert_eq!(turns.len(), 2, "a tool result doesn't start a turn");
        let kinds: Vec<&str> = turns[0]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            ["text", "thinking", "tool_call", "tool_result", "text"]
        );
        assert_eq!(turns[0]["items"][2]["tool"], "Bash");
        assert_eq!(turns[1]["n"], 2);
        assert_eq!(turns[0]["tool_count"], 1);
        assert_eq!(turns[0]["subagent_count"], 0);
        // No parseable timestamps: duration unknown.
        assert!(turns[0]["duration_ms"].is_null());
        assert!(turns[0]["items"][0]["ts"].is_null());
    }

    #[test]
    fn task_notifications_become_bg_events() {
        let note = |status: &str| {
            format!(
                "<task-notification>\n<task-id>bg1</task-id>\n<tool-use-id>tu1</tool-use-id>\n{status}<summary>Build finished, token sk-ant-api03-{}</summary>\n</task-notification>",
                "a".repeat(90)
            )
        };
        let t = [
            json!({"type": "user", "message": {"content": "start"}}),
            json!({"type": "user", "origin": {"kind": "task-notification"}, "timestamp": "2026-01-01T00:00:10Z", "message": {"content": note("<status>completed</status>\n")}}),
            json!({"type": "user", "message": {"content": note("")}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let turns = transcript_items(&t);
        assert_eq!(turns.len(), 3, "each notification starts a turn");
        let i = &turns[1]["items"][0];
        assert_eq!(i["kind"], "bg_event");
        assert_eq!(i["id"], "bg1");
        assert_eq!(i["tool_use_id"], "tu1");
        assert_eq!(i["status"], "completed");
        assert!(i["ts"].is_i64(), "ts is added like other items");
        assert!(
            !i["summary"]
                .as_str()
                .unwrap()
                .contains("aaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(turns[1]["tool_count"], 0);
        assert_eq!(turns[2]["items"][0]["status"], "event");
        assert_eq!(turns[2]["items"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn claude_turn_timing_and_subagents() {
        let t = [
            json!({"type": "user", "message": {"content": "go"}, "timestamp": "2026-10-06T12:00:00.000Z"}),
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Task", "id": "t1", "input": {"prompt": "explore"}}]}, "timestamp": "2026-10-06T12:00:01.500Z"}),
            json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "name": "Agent", "id": "t2", "input": {}}, {"type": "tool_use", "name": "Read", "id": "t3", "input": {}}]}, "timestamp": "2026-10-06T12:00:02.000Z"}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "done"}]}, "timestamp": "2026-10-06T12:00:09.250Z"}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let turns = transcript_items(&t);
        assert_eq!(turns.len(), 1);
        let turn = &turns[0];
        assert_eq!(turn["ts"], "2026-10-06T12:00:00.000Z", "turn ts unchanged");
        assert_eq!(turn["duration_ms"], 9250);
        assert_eq!(turn["tool_count"], 3);
        assert_eq!(turn["subagent_count"], 2);
        let start = crate::agents::usage::parse_rfc3339_ms("2026-10-06T12:00:00.000Z").unwrap();
        assert_eq!(turn["items"][0]["ts"], start);
        assert_eq!(turn["items"][1]["ts"], start + 1500);
    }

    #[test]
    fn codex_rollout_items() {
        let t = [
            json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}}),
            json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell", "arguments": "{\"cmd\":[\"ls\"]}", "call_id": "c1"}}),
            json!({"type": "response_item", "payload": {"type": "function_call_output", "call_id": "c1", "output": "a b"}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let turns = transcript_items(&t);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["items"].as_array().unwrap().len(), 3);
        assert_eq!(turns[0]["tool_count"], 1);
        assert!(turns[0]["duration_ms"].is_null());
    }

    #[test]
    fn codex_transcript_displays_text_instead_of_transport_wrappers() {
        let script =
            "const result = await tools.exec_command({cmd: \"cargo test\"});\ntext(result);";
        let output = format!("first line\n{}\nlast line", "x".repeat(220));
        let records = [
            json!({"type":"response_item", "payload":{"type":"message", "role":"user", "content":[{"type":"input_text","text":"check"}]}}),
            json!({"type":"response_item", "payload":{"type":"reasoning", "summary":[]}}),
            json!({"type":"response_item", "payload":{"type":"reasoning", "summary":[{"type":"summary_text","text":"Check the build"}]}}),
            json!({"type":"response_item", "payload":{"type":"custom_tool_call", "name":"exec", "input":script, "call_id":"c1"}}),
            json!({"type":"response_item", "payload":{"type":"custom_tool_call_output", "call_id":"c1", "output":[{"type":"input_text","text":json!({"chunk_id":"c", "wall_time_seconds":0.5, "exit_code":0, "output":output}).to_string()}]}}),
        ];
        let text = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let turns = transcript_items(&text);
        let items = turns[0]["items"].as_array().unwrap();
        assert_eq!(items.len(), 4, "empty reasoning has no row");
        assert_eq!(items[1]["text"], "Check the build");
        assert_eq!(items[2]["text"], script);
        assert_eq!(items[3]["text"], output);
        assert!(items[3]["summary"].as_str().unwrap().chars().count() <= 160);
        assert!(!items[3]["summary"].as_str().unwrap().contains("chunk_id"));
        let detail = transcript_detail(&"🙂".repeat(9000));
        assert_eq!(detail.chars().count(), 8194);
        assert!(detail.ends_with("\n…"));
        assert_eq!(
            transcript_output(&json!({"output":"domain data"}), 0),
            r#"{"output":"domain data"}"#
        );
        assert_eq!(
            transcript_output(&json!([{"text":"domain data"}]), 0),
            r#"[{"text":"domain data"}]"#
        );
    }

    #[test]
    fn codex_transcript_preserves_failure_status_when_unwrapping_output() {
        for output in [
            json!({"exit_code":1, "output":""}),
            json!({"metadata":{"exit_code":2}, "output":"failed"}),
            json!([{"type":"input_text", "text":json!({"chunk_id":"x", "exit_code":3, "output":""}).to_string()}]),
        ] {
            let result = transcript_result(&json!(output.to_string()), &json!("c1"), false);
            assert_eq!(result["error"], true);
            assert!(
                result["text"]
                    .as_str()
                    .unwrap()
                    .contains("Command exited with code")
            );
        }
        assert_eq!(
            transcript_result(&json!({"exit_code":0,"output":"ok"}), &json!("c2"), false)["error"],
            false
        );
        let data = json!({"output":"domain data", "metadata":{"source":"example"}});
        assert_eq!(transcript_output(&data, 0), data.to_string());
        let stdout = json!({"output":"domain data", "exit_code":12}).to_string();
        let result = transcript_result(
            &json!({"exit_code":0, "output":stdout}),
            &json!("c3"),
            false,
        );
        assert_eq!(result["text"], stdout);
        assert_eq!(result["error"], false);
    }

    #[test]
    fn codex_transcript_uses_the_harness_elapsed_time() {
        let text = [
            json!({"timestamp":"2026-01-01T00:00:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]}}),
            json!({"timestamp":"2026-01-01T00:57:08Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done"}]}}),
            json!({"timestamp":"2026-01-01T01:00:13Z","type":"event_msg","payload":{"type":"task_complete","duration_ms":3613322}}),
        ].iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        assert_eq!(transcript_items(&text)[0]["duration_ms"], 3613322);
    }

    #[test]
    fn codex_turn_timing_and_subagents() {
        let t = [
            json!({"timestamp": "2026-10-06T08:00:00.100Z", "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}}),
            json!({"timestamp": "2026-10-06T08:00:01.000Z", "type": "response_item", "payload": {"type": "function_call", "name": "spawn_agent", "arguments": "{}", "call_id": "c1"}}),
            json!({"timestamp": "2026-10-06T08:00:02.000Z", "type": "response_item", "payload": {"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "c2"}}),
            json!({"timestamp": "2026-10-06T08:00:03.100Z", "type": "response_item", "payload": {"type": "function_call_output", "call_id": "c2", "output": "ok"}}),
            json!({"timestamp": "2026-10-06T08:01:00.000Z", "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "again"}]}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let turns = transcript_items(&t);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0]["duration_ms"], 3000);
        assert_eq!(turns[0]["tool_count"], 2);
        assert_eq!(turns[0]["subagent_count"], 1);
        assert_eq!(turns[0]["items"][3]["ts"].as_i64().unwrap() % 1000, 100);
        assert_eq!(turns[1]["duration_ms"], 0);
        assert_eq!(turns[1]["tool_count"], 0);
    }

    #[test]
    fn transcript_images_are_items_with_a_fetchable_ref() {
        let data = "iVBORw0KGgo=";
        let t = [
            json!({"type": "user", "message": {"content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": data}},
                {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}},
            ]}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "seen"}]}}),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/svg+xml", "data": data}},
                {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "/9j/4A=="}},
            ]}]}}),
            // pi: data + mimeType on the block.
            json!({"type": "message", "message": {"role": "user", "content": [{"type": "image", "data": data, "mimeType": "image/gif"}]}}),
            // Codex: a data URL.
            json!({"type": "response_item", "payload": {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "shot"},
                {"type": "input_image", "image_url": format!("data:image/webp;base64,{data}")},
            ]}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let turns = transcript_items(&t);
        let images: Vec<&Value> = turns
            .iter()
            .flat_map(|t| t["items"].as_array().unwrap())
            .filter(|i| i["kind"] == "image")
            .collect();
        let refs: Vec<(&str, &str)> = images
            .iter()
            .map(|i| (i["mime"].as_str().unwrap(), i["ref"].as_str().unwrap()))
            .collect();
        assert_eq!(
            refs,
            [
                ("image/png", "0:0"),
                ("image/jpeg", "2:0"),
                ("image/gif", "3:0"),
                ("image/webp", "4:0"),
            ]
        );
        assert!(
            images
                .iter()
                .all(|i| i.get("data").is_none() && i.get("data_b64").is_none())
        );
        // The ref fetches the bytes; nothing else does.
        let got = transcript_image(&t, "0:0").unwrap();
        assert_eq!(
            (got["mime"].as_str(), got["data_b64"].as_str()),
            (Some("image/png"), Some(data))
        );
        assert_eq!(transcript_image(&t, "2:0").unwrap()["mime"], "image/jpeg");
        assert_eq!(transcript_image(&t, "4:0").unwrap()["mime"], "image/webp");
        for bad in ["0:1", "9:0", "x", "1:0"] {
            assert!(transcript_image(&t, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn transcript_image_over_the_cap_is_refused() {
        let big = "A".repeat(MAX_TRANSCRIPT_IMAGE / 3 * 4 + 8);
        let t = json!({"type": "user", "message": {"content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": big}}]}}).to_string();
        let e = transcript_image(&t, "0:0").unwrap_err();
        assert_eq!(e.data.kind, "unsupported");
    }

    #[test]
    fn styled_runs_skip_plain_spans() {
        use vk_proto::render::{Span, Style};
        let row = Row {
            spans: vec![
                Span {
                    style: Style::default(),
                    text: "ab".into(),
                    cols: 2,
                },
                Span {
                    style: Style {
                        fg: Color::Indexed(1),
                        attrs: attr::BOLD,
                        ..Default::default()
                    },
                    text: "cd".into(),
                    cols: 2,
                },
            ],
            wrapped: false,
            ..Default::default()
        };
        let v = styled_rows(&[row]);
        assert_eq!(v[0]["text"], "abcd");
        assert_eq!(v[0]["runs"].as_array().unwrap().len(), 1);
        assert_eq!(v[0]["runs"][0]["start"], 2);
        assert_eq!(v[0]["runs"][0]["bold"], true);
    }
}
