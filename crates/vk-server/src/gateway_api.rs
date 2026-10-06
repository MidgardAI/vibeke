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
}

fn state(server: &Server) -> &State {
    &server.gateway
}

/// Runs before normal dispatch. `None` lets the regular handler run.
pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if ctx.kind == "gateway"
        && let Some(r) = audit(server, ctx, method, p)
    {
        return Some(r);
    }
    Some(match method {
        "pane.read" if s(p, "source") == Some("styled") => styled(server, ctx, p),
        "agent.transcript"
            if p.get("before").is_some()
                || p.get("items").and_then(Value::as_bool) == Some(true)
                || ctx.kind == "gateway" =>
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

/// Items from one transcript line, for Claude (`message.content[]`) and Codex rollout
/// (`response_item` payloads). Returns (is_user_prompt, items).
fn line_items(v: &Value) -> (bool, Vec<Value>) {
    let mut items = Vec::new();
    let mut user_prompt = false;
    if let Some(msg) = v.get("message") {
        let role = v.get("type").and_then(Value::as_str).unwrap_or("");
        match msg.get("content") {
            Some(Value::String(t)) => {
                user_prompt = role == "user";
                items.push(json!({"kind": "text", "role": role, "text": t}));
            }
            Some(Value::Array(a)) => {
                for c in a {
                    match c.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            user_prompt |= role == "user";
                            items.push(json!({"kind": "text", "role": role, "text": c.get("text")}));
                        }
                        Some("thinking") => items.push(json!({"kind": "thinking", "text": c.get("thinking")})),
                        Some("tool_use") => items.push(json!({"kind": "tool_call", "tool": c.get("name"), "summary": summarize(c.get("input").unwrap_or(&Value::Null)), "id": c.get("id")})),
                        Some("tool_result") => items.push(json!({"kind": "tool_result", "summary": summarize(c.get("content").unwrap_or(&Value::Null)), "id": c.get("tool_use_id"), "error": c.get("is_error")})),
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
                let text: String = pl.get("content").and_then(Value::as_array).map(|a| a.iter().filter_map(|c| c.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")).unwrap_or_default();
                if !text.is_empty() {
                    user_prompt = role == "user";
                    items.push(json!({"kind": "text", "role": role, "text": text}));
                }
            }
            Some("reasoning") => items.push(json!({"kind": "thinking", "text": pl.get("summary").map(summarize)})),
            Some("function_call") | Some("local_shell_call") | Some("custom_tool_call") => items.push(json!({"kind": "tool_call", "tool": pl.get("name"), "summary": summarize(pl.get("arguments").or(pl.get("action")).unwrap_or(&Value::Null)), "id": pl.get("call_id")})),
            Some("function_call_output") | Some("custom_tool_call_output") => items.push(json!({"kind": "tool_result", "summary": summarize(pl.get("output").unwrap_or(&Value::Null)), "id": pl.get("call_id")})),
            _ => {}
        }
    }
    (user_prompt, items)
}

/// Turns numbered from the start of the transcript (stable `n` across calls). A turn starts at
/// a user prompt (not a tool result).
pub fn transcript_items(text: &str) -> Vec<Value> {
    let mut turns: Vec<Value> = Vec::new();
    for l in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        let (prompt, items) = line_items(&v);
        if items.is_empty() {
            continue;
        }
        if prompt || turns.is_empty() {
            turns.push(json!({"n": turns.len() as u64 + 1, "ts": v.get("timestamp"), "items": []}));
        }
        let last = turns.last_mut().unwrap();
        last["items"].as_array_mut().unwrap().extend(items);
    }
    turns
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
    Ok(json!({"run": run.id, "turns": page, "next_before": next_before}))
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
    // Devices reported by gateways that are still connected (stale reporters dropped).
    let mut dev = state(server).devices.lock().unwrap();
    dev.retain(|id, _| clients.contains_key(id));
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

/// Only a TUI (chrome, out of band of any PTY) may answer a confirmation.
fn confirm_answer(server: &Server, ctx: &Ctx, p: &Value) -> R {
    if ctx.pane_scope.is_some() || !matches!(ctx.kind.as_str(), "tui" | "anonymous") || ctx.remote {
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
        };
        let v = styled_rows(&[row]);
        assert_eq!(v[0]["text"], "abcd");
        assert_eq!(v[0]["runs"].as_array().unwrap().len(), 1);
        assert_eq!(v[0]["runs"][0]["start"], 2);
        assert_eq!(v[0]["runs"][0]["bold"], true);
    }
}
