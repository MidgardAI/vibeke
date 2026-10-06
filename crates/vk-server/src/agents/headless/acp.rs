//! Generic ACP agents headless (04 §6.6): the agent runs under a pipe-mode holder and the
//! server is the ACP client (the pane-hosted `vibeke acp-host` remains the TUI-mode path). The
//! mapping is the host's (`agents/acp.rs`): `session/new|load` → `SessionStart`,
//! `session/prompt` → turn, `session/update` → items, `session/request_permission` → approval
//! answered with `{outcome: {outcome: "selected", optionId}}`, `fs/read_text_file` /
//! `fs/write_text_file` served inside the session cwd, `session/cancel` → interrupt.
//!
//! **Reconcile.** A restarted server replays the journal; when the ring no longer reaches the
//! session start (`gap`) and the agent advertises `loadSession`, the adapter sends
//! `session/load`: the agent replays the conversation as `session/update`s, which rebuild the
//! transcript without re-emitting events (the run already saw them). `agent.resume` of an ACP
//! run starts a new agent process with `session/load` as well.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Req {
    Initialize,
    New,
    Load,
    Prompt,
    Other,
}

pub struct Acp {
    cwd: std::path::PathBuf,
    resume: Option<String>,
    initialized: bool,
    load_session: bool,
    session: Option<String>,
    busy: bool,
    /// A `session/load` is replaying history: render only, no events.
    loading: bool,
    identified: bool,
    next: u64,
    inflight: HashMap<u64, Req>,
    /// Unanswered permission requests: (id, params).
    pending: Vec<(Value, Value)>,
    /// Unanswered requests answered automatically: (id, method, params).
    auto: Vec<(Value, String, Value)>,
    /// toolCallId → (title, rawInput, kind).
    tools: HashMap<String, (String, Value, String)>,
    turn_text: String,
}

fn id_str(id: &Value) -> String {
    id.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| id.to_string())
}

impl Acp {
    pub fn new(rec: &Record) -> Acp {
        // Rebuilt after a restart: the session was established before, even if the journal no
        // longer shows the handshake.
        let est = rec.established();
        Acp {
            cwd: std::path::PathBuf::from(&rec.cwd),
            resume: rec.session.clone().filter(|_| rec.resume),
            initialized: est,
            load_session: est,
            session: rec.session.clone().filter(|_| est || !rec.resume),
            busy: false,
            loading: false,
            identified: est,
            next: 1,
            inflight: HashMap::new(),
            pending: vec![],
            auto: vec![],
            tools: HashMap::new(),
            turn_text: String::new(),
        }
    }

    fn request(&mut self, cx: &mut Cx, method: &str, params: Value) {
        let id = self.next;
        self.next += 1;
        cx.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    }

    fn open_session(&mut self, cx: &mut Cx) {
        let cwd = self.cwd.to_string_lossy().to_string();
        match self.resume.clone() {
            Some(sid) if self.load_session => self.request(
                cx,
                "session/load",
                json!({"sessionId": sid, "cwd": cwd, "mcpServers": []}),
            ),
            _ => self.request(cx, "session/new", json!({"cwd": cwd, "mcpServers": []})),
        }
    }

    fn within_cwd(&self, path: &str) -> Option<std::path::PathBuf> {
        let p = std::path::Path::new(path);
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        };
        let root = self.cwd.canonicalize().unwrap_or_else(|_| self.cwd.clone());
        let parent = p.parent()?.canonicalize().ok()?;
        let full = parent.join(p.file_name()?);
        full.starts_with(&root).then_some(full)
    }

    /// The response to an automatic request (`fs/*`, unsupported methods).
    fn auto_response(&self, id: &Value, method: &str, p: &Value) -> Value {
        let err = |code: i64, msg: &str| json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}});
        match method {
            "fs/read_text_file" => {
                let path = p.get("path").and_then(Value::as_str).unwrap_or("");
                match self
                    .within_cwd(path)
                    .and_then(|f| std::fs::read_to_string(f).ok())
                {
                    Some(text) => {
                        let line = p.get("line").and_then(Value::as_u64);
                        let limit = p.get("limit").and_then(Value::as_u64);
                        let content = match (line, limit) {
                            (None, None) => text,
                            _ => text
                                .lines()
                                .skip(line.unwrap_or(1).saturating_sub(1) as usize)
                                .take(limit.unwrap_or(u64::MAX) as usize)
                                .collect::<Vec<_>>()
                                .join("\n"),
                        };
                        json!({"jsonrpc": "2.0", "id": id, "result": {"content": content}})
                    }
                    None => err(-32002, "file not readable or outside the session cwd"),
                }
            }
            "fs/write_text_file" => {
                let path = p.get("path").and_then(Value::as_str).unwrap_or("");
                let content = p.get("content").and_then(Value::as_str).unwrap_or("");
                match self.within_cwd(path) {
                    Some(f) if std::fs::write(&f, content).is_ok() => {
                        json!({"jsonrpc": "2.0", "id": id, "result": null})
                    }
                    _ => err(-32002, "write refused: outside the session cwd"),
                }
            }
            _ => err(-32601, "method not supported by vibeke"),
        }
    }

    fn interaction(&self, id: &Value, p: &Value) -> Interaction {
        let mut p = p.clone();
        // A permission may refer to a tool call announced earlier with more detail.
        let tcid = p
            .pointer("/toolCall/toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let (Some((title, raw, _)), Some(o)) = (
            self.tools.get(&tcid).cloned(),
            p.get_mut("toolCall").and_then(Value::as_object_mut),
        ) {
            o.entry("title").or_insert(json!(title));
            o.entry("rawInput").or_insert(raw);
        }
        let mut it = super::super::acp::permission_interaction(&p);
        it.id = crate::core::ulid();
        it.native_ref = Some(format!("rpc:{}", id_str(id)));
        it.answer_channel = AnswerChannel::Native;
        it.answerable = true;
        it.source = StateSource::Structured;
        it.confidence = 1.0;
        it
    }

    fn on_update(&mut self, cx: &mut Cx, params: &Value) {
        let u = &params["update"];
        let quiet = self.loading;
        let sig = |cx: &mut Cx, e: &str, p: Value| {
            if !quiet {
                cx.signal(e, p);
            }
        };
        match u.get("sessionUpdate").and_then(Value::as_str).unwrap_or("") {
            "agent_message_chunk" => {
                if let Some(t) = u.pointer("/content/text").and_then(Value::as_str) {
                    if !quiet {
                        self.turn_text.push_str(t);
                    }
                    cx.render(t.to_string());
                }
            }
            "user_message_chunk" => {
                if let Some(t) = u.pointer("/content/text").and_then(Value::as_str) {
                    cx.render(format!("\n› {t}\n"));
                }
            }
            "tool_call" => {
                let id = u
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let title = u
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let kind = u
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("other")
                    .to_string();
                let raw = u.get("rawInput").cloned().unwrap_or(json!({}));
                let mut input = raw.clone();
                if let (Some(o), Some(path)) = (
                    input.as_object_mut(),
                    u.pointer("/locations/0/path").and_then(Value::as_str),
                ) {
                    o.entry("file_path").or_insert(json!(path));
                }
                cx.render(format!("\n⏺ {title}\n"));
                sig(
                    cx,
                    "PreToolUse",
                    json!({"tool_name": tool_name(&kind, &title), "tool_input": input, "tool_use_id": id}),
                );
                self.tools.insert(id, (title, raw, kind));
                if matches!(
                    u.get("status").and_then(Value::as_str),
                    Some("completed" | "failed")
                ) {
                    self.tool_done(cx, u, quiet);
                }
            }
            "tool_call_update" => {
                if matches!(
                    u.get("status").and_then(Value::as_str),
                    Some("completed" | "failed")
                ) {
                    self.tool_done(cx, u, quiet);
                }
            }
            "plan" => {
                let mut s = String::from("\nplan:\n");
                for e in u
                    .get("entries")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
                {
                    let mark = match e.get("status").and_then(Value::as_str) {
                        Some("completed") => "✓",
                        Some("in_progress") => "▸",
                        _ => "·",
                    };
                    s.push_str(&format!(
                        "  {mark} {}\n",
                        e.get("content").and_then(Value::as_str).unwrap_or("")
                    ));
                }
                cx.render(s);
            }
            _ => {}
        }
    }

    fn tool_done(&mut self, cx: &mut Cx, u: &Value, quiet: bool) {
        let id = u
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let failed = u.get("status").and_then(Value::as_str) == Some("failed");
        let (title, raw, known) = self
            .tools
            .remove(&id)
            .unwrap_or_else(|| ("tool".into(), json!({}), "other".into()));
        let kind = u
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or(&known)
            .to_string();
        let mut input = raw;
        if let (Some(o), Some(path)) = (
            input.as_object_mut(),
            u.pointer("/locations/0/path").and_then(Value::as_str),
        ) {
            o.entry("file_path").or_insert(json!(path));
        }
        cx.render(format!("  {} {title}\n", if failed { "✗" } else { "✓" }));
        if !quiet {
            cx.signal(
                if failed { "PostToolUseFailure" } else { "PostToolUse" },
                json!({"tool_name": tool_name(&kind, &title), "tool_use_id": id, "tool_input": input}),
            );
        }
    }
}

fn tool_name(kind: &str, title: &str) -> String {
    match kind {
        "execute" => "Bash".into(),
        "edit" => "Edit".into(),
        "read" => "Read".into(),
        "search" => "Grep".into(),
        "fetch" => "WebFetch".into(),
        "delete" => "Delete".into(),
        "move" => "Move".into(),
        "think" => "Think".into(),
        _ => title
            .split_whitespace()
            .next()
            .unwrap_or("tool")
            .to_string(),
    }
}

impl Adapter for Acp {
    fn start(&mut self, cx: &mut Cx) {
        self.request(
            cx,
            "initialize",
            json!({"protocolVersion": super::super::acp::PROTOCOL_VERSION, "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": false}}),
        );
    }

    fn on_frame(&mut self, cx: &mut Cx, v: &Value) {
        let method = v.get("method").and_then(Value::as_str);
        match (method, v.get("id")) {
            (None, Some(id)) => {
                let Some(kind) = id.as_u64().and_then(|i| self.inflight.remove(&i)) else {
                    return;
                };
                if let Some(e) = v.get("error") {
                    let msg = e.get("message").and_then(Value::as_str).unwrap_or("error");
                    cx.render(format!("\n! {msg}\n"));
                    match kind {
                        Req::Prompt => {
                            self.busy = false;
                            cx.signal("StopFailure", json!({"error_type": msg}));
                        }
                        Req::Load => {
                            self.loading = false;
                            if self.session.is_none() {
                                self.resume = None;
                                self.open_session(cx);
                            }
                        }
                        _ => {}
                    }
                    return;
                }
                let r = v.get("result").cloned().unwrap_or(Value::Null);
                match kind {
                    Req::Initialize => {
                        self.initialized = true;
                        self.load_session = r
                            .pointer("/agentCapabilities/loadSession")
                            .and_then(Value::as_bool)
                            .unwrap_or(self.load_session);
                        if self.session.is_none() {
                            self.open_session(cx);
                        }
                    }
                    Req::New | Req::Load => {
                        let resumed = kind == Req::Load;
                        self.loading = false;
                        let sid = r
                            .get("sessionId")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .or_else(|| self.resume.clone())
                            .or_else(|| self.session.clone());
                        if let Some(s) = sid {
                            self.session = Some(s.clone());
                            cx.session(&s);
                            if !self.identified {
                                self.identified = true;
                                cx.render(format!("session {s}\n› "));
                                cx.signal(
                                    "SessionStart",
                                    json!({"session_id": s, "cwd": self.cwd.to_string_lossy(), "source": if resumed { "resume" } else { "startup" }}),
                                );
                            } else {
                                cx.render("\n(session reloaded)\n› ");
                            }
                        }
                    }
                    Req::Prompt => {
                        self.busy = false;
                        let stop = r
                            .get("stopReason")
                            .and_then(Value::as_str)
                            .unwrap_or("end_turn")
                            .to_string();
                        if let Some(u) = r.get("usage").or_else(|| r.pointer("/_meta/usage")) {
                            let n = |k: &[&str]| {
                                k.iter()
                                    .find_map(|k| u.get(*k).and_then(Value::as_u64))
                                    .unwrap_or(0)
                            };
                            cx.usage(
                                RunUsage {
                                    input_tokens: n(&["inputTokens", "input_tokens"]),
                                    output_tokens: n(&["outputTokens", "output_tokens"]),
                                    cache_read_tokens: n(&["cachedReadTokens", "cacheReadTokens"]),
                                    cache_write_tokens: n(&[
                                        "cachedWriteTokens",
                                        "cacheWriteTokens",
                                    ]),
                                    cost_usd: None,
                                    model: None,
                                    source: "acp".into(),
                                    updated_at_ms: 0,
                                },
                                false,
                            );
                        }
                        let msg = std::mem::take(&mut self.turn_text).trim().to_string();
                        if stop == "refusal" || stop == "max_tokens" {
                            cx.signal("StopFailure", json!({"error_type": stop}));
                        } else {
                            cx.signal(
                                "Stop",
                                json!({"session_id": self.session, "last_assistant_message": msg, "stop_reason": stop}),
                            );
                        }
                        cx.render(format!("\n[{stop}]\n› "));
                    }
                    Req::Other => {}
                }
            }
            (Some("session/update"), None) => {
                let p = v.get("params").cloned().unwrap_or(Value::Null);
                self.on_update(cx, &p);
            }
            (Some("session/request_permission"), Some(id)) => {
                let p = v.get("params").cloned().unwrap_or(Value::Null);
                if !self.pending.iter().any(|(i, _)| i == id) {
                    self.pending.push((id.clone(), p.clone()));
                }
                cx.open(self.interaction(id, &p));
            }
            (Some(m), Some(id)) => {
                let p = v.get("params").cloned().unwrap_or(Value::Null);
                self.auto.push((id.clone(), m.to_string(), p.clone()));
                cx.write(self.auto_response(id, m, &p));
            }
            _ => {}
        }
    }

    fn on_sent(&mut self, cx: &mut Cx, v: &Value) {
        let method = v.get("method").and_then(Value::as_str);
        match (method, v.get("id")) {
            (Some(m), Some(id)) => {
                let kind = match m {
                    "initialize" => Req::Initialize,
                    "session/new" => Req::New,
                    "session/load" => {
                        self.loading = true;
                        Req::Load
                    }
                    "session/prompt" => {
                        self.busy = true;
                        self.turn_text.clear();
                        let text: String = v
                            .pointer("/params/prompt")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default();
                        cx.render(format!("\n› {text}\n"));
                        cx.signal(
                            "UserPromptSubmit",
                            json!({"prompt": text, "session_id": self.session}),
                        );
                        Req::Prompt
                    }
                    _ => Req::Other,
                };
                if let Some(n) = id.as_u64() {
                    self.inflight.insert(n, kind);
                    self.next = self.next.max(n + 1);
                }
            }
            (None, Some(id)) => {
                self.pending.retain(|(i, _)| i != id);
                self.auto.retain(|(i, _, _)| i != id);
            }
            _ => {}
        }
    }

    fn prompt(&mut self, cx: &mut Cx, text: &str, _mode: PromptMode) -> Result<(), String> {
        let Some(sid) = self.session.clone() else {
            return Err("the ACP session is not open yet".into());
        };
        if self.busy {
            return Err("a turn is in progress (ACP has no steering); interrupt first".into());
        }
        let id = self.next;
        self.next += 1;
        cx.write_prompt(
            json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt", "params": {"sessionId": sid, "prompt": [{"type": "text", "text": text}]}}),
            text,
        );
        Ok(())
    }

    fn interrupt(&mut self, cx: &mut Cx) {
        if let Some(s) = self.session.clone() {
            cx.write(
                json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": s}}),
            );
        }
    }

    fn answer(&mut self, cx: &mut Cx, native_ref: &str, it: &Interaction, a: &Answer) -> bool {
        let Some((id, _)) = self
            .pending
            .iter()
            .find(|(i, _)| format!("rpc:{}", id_str(i)) == native_ref)
            .cloned()
        else {
            return false;
        };
        let result = super::super::acp::decision_json(it, a);
        cx.write(json!({"jsonrpc": "2.0", "id": id, "result": result}));
        true
    }

    fn pending(&self) -> Vec<Pending> {
        let mut v: Vec<Pending> = self
            .pending
            .iter()
            .map(|(id, p)| Pending {
                native_ref: format!("rpc:{}", id_str(id)),
                interaction: Some(self.interaction(id, p)),
            })
            .collect();
        v.extend(self.auto.iter().map(|(id, _, _)| Pending {
            native_ref: format!("rpc:{}", id_str(id)),
            interaction: None,
        }));
        v
    }

    fn reconcile(&mut self, cx: &mut Cx, gap: bool) {
        let has = |k: Req| self.inflight.values().any(|x| *x == k);
        if !self.initialized {
            if !has(Req::Initialize) {
                self.start(cx);
            }
        } else if self.session.is_none() {
            if !has(Req::New) && !has(Req::Load) {
                self.open_session(cx);
            }
        } else if gap && self.load_session && !self.busy && !has(Req::Load) {
            // The journal lost the session start: let the agent replay it (04 §6.6).
            let sid = self.session.clone().unwrap_or_default();
            let cwd = self.cwd.to_string_lossy().to_string();
            self.request(
                cx,
                "session/load",
                json!({"sessionId": sid, "cwd": cwd, "mcpServers": []}),
            );
        }
        for (id, m, p) in self.auto.clone() {
            cx.write(self.auto_response(&id, &m, &p));
        }
    }

    fn ready(&self) -> bool {
        self.session.is_some() && !self.loading
    }

    fn busy(&self) -> bool {
        self.busy
    }
}
