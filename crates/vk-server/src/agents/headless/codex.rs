//! Codex headless (04 §6.2): `codex app-server`, JSON-RPC over stdio (the `"jsonrpc"` member is
//! omitted, as the app-server protocol does).
//!
//! | app-server | Vibeke |
//! |---|---|
//! | `initialize` → `initialized`, `thread/start` (or `thread/resume`) | `SessionStart` (thread id) |
//! | `turn/start` written by Vibeke | `UserPromptSubmit` (turn); `turn/steer` mid-turn |
//! | `item/started` / `item/completed` (`commandExecution`, `fileChange`, `mcpToolCall`) | `PreToolUse` / `PostToolUse(Failure)` |
//! | `item/completed` `agentMessage` | transcript + last message |
//! | `turn/completed {status}` | `Stop` / `Interrupt` / `StopFailure` |
//! | `thread/tokenUsage/updated` | usage (session total) |
//! | `account/rateLimits/updated` | run rate limit (most constrained window; 100 % → rate limited) |
//! | `item/commandExecution/requestApproval`, `item/fileChange/requestApproval`, legacy `execCommandApproval`/`applyPatchApproval`, `item/permissions/requestApproval` | approval: `accept` / `acceptForSession` / `decline` |
//! | `item/tool/requestUserInput`, `mcpServer/elicitation/request` | question |
//! | `serverRequest/resolved` | resolved by the harness |
//! | `agent.interrupt` | `turn/interrupt` |
//!
//! Reconcile after a restart: `thread/read` (a turn the journal shows running but the thread
//! reports idle is completed), `thread/resume` when the thread is no longer loaded.

use super::*;

pub struct Codex {
    cwd: String,
    resume: Option<String>,
    initialized: bool,
    thread: Option<String>,
    turn: Option<String>,
    busy: bool,
    next: u64,
    /// Our requests in flight: id → method (learned from the stdin journal).
    inflight: HashMap<u64, String>,
    /// Unanswered server requests: (id, method, params).
    pending: Vec<(Value, String, Value)>,
    /// Requests refused automatically (unknown methods).
    auto: Vec<Value>,
    /// item id → (tool, input), from `item/started`.
    items: HashMap<String, (String, Value)>,
    last_msg: Option<String>,
    identified: bool,
    /// The last rate-limit snapshot had a window at 100 %.
    limited: bool,
}

fn id_str(id: &Value) -> String {
    id.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| id.to_string())
}

impl Codex {
    pub fn new(rec: &Record) -> Codex {
        // Rebuilt after a restart: the thread was open before, even if the journal no longer
        // shows the handshake (ring overflow).
        let est = rec.established();
        Codex {
            cwd: rec.cwd.clone(),
            resume: rec.session.clone().filter(|_| rec.resume),
            initialized: est,
            thread: rec.session.clone().filter(|_| est),
            turn: None,
            busy: false,
            next: 1,
            inflight: HashMap::new(),
            pending: vec![],
            auto: vec![],
            items: HashMap::new(),
            last_msg: None,
            identified: est,
            limited: false,
        }
    }

    fn request(&mut self, cx: &mut Cx, method: &str, params: Value) {
        let id = self.next;
        self.next += 1;
        cx.write(json!({"id": id, "method": method, "params": params}));
    }

    fn inflight_has(&self, method: &str) -> bool {
        self.inflight.values().any(|m| m == method)
    }

    fn open_thread(&mut self, cx: &mut Cx) {
        match self.resume.clone() {
            Some(t) => self.request(cx, "thread/resume", json!({"threadId": t})),
            None => self.request(cx, "thread/start", json!({"cwd": self.cwd})),
        }
    }

    fn tool_of(item: &Value) -> Option<(String, Value)> {
        let ty = item.get("type").and_then(Value::as_str)?;
        Some(match ty {
            "commandExecution" => {
                let cmd = item
                    .get("command")
                    .map(|c| {
                        c.as_str().map(str::to_string).unwrap_or_else(|| {
                            c.as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(Value::as_str)
                                        .collect::<Vec<_>>()
                                        .join(" ")
                                })
                                .unwrap_or_default()
                        })
                    })
                    .unwrap_or_default();
                ("Bash".into(), json!({"command": cmd}))
            }
            "fileChange" => {
                let path = item
                    .pointer("/changes/0/path")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                ("Edit".into(), json!({"file_path": path}))
            }
            "mcpToolCall" => (
                item.get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("mcp")
                    .to_string(),
                item.get("arguments").cloned().unwrap_or(json!({})),
            ),
            "webSearch" => ("WebSearch".into(), json!({"query": item.get("query")})),
            _ => return None,
        })
    }

    /// The unified diffs of a `fileChange` item (`changes[] {path, kind, diff}`).
    fn item_diff(item: &Value) -> Option<String> {
        let changes = item.get("changes")?.as_array()?;
        let mut out = String::new();
        for c in changes {
            let Some(d) = c.get("diff").and_then(Value::as_str) else {
                continue;
            };
            if changes.len() > 1
                && let Some(p) = c.get("path").and_then(Value::as_str)
            {
                out.push_str(&format!("--- {p}\n"));
            }
            out.push_str(d);
            if !d.ends_with('\n') {
                out.push('\n');
            }
        }
        (!out.is_empty()).then_some(out)
    }

    fn interaction(&self, id: &Value, method: &str, p: &Value) -> Option<Interaction> {
        let native = format!("rpc:{}", id_str(id));
        let reason = p.get("reason").and_then(Value::as_str);
        Some(match method {
            "item/commandExecution/requestApproval" | "execCommandApproval" => {
                let cmd = p
                    .get("command")
                    .map(|c| {
                        c.as_str().map(str::to_string).unwrap_or_else(|| {
                            c.as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(Value::as_str)
                                        .collect::<Vec<_>>()
                                        .join(" ")
                                })
                                .unwrap_or_default()
                        })
                    })
                    .unwrap_or_default();
                let mut it = approval(native, "Bash", &json!({"command": cmd}), None);
                it.body_md = reason.map(str::to_string);
                it
            }
            "item/fileChange/requestApproval" | "applyPatchApproval" => {
                let path = p
                    .get("itemId")
                    .and_then(Value::as_str)
                    .and_then(|i| self.items.get(i))
                    .and_then(|(_, input)| input.get("file_path").and_then(Value::as_str))
                    .map(str::to_string)
                    .or_else(|| {
                        p.get("fileChanges")
                            .and_then(Value::as_object)
                            .and_then(|m| m.keys().next().cloned())
                    })
                    .unwrap_or_default();
                let mut it = approval(native, "Edit", &json!({"file_path": path}), None);
                it.body_md = reason.map(str::to_string);
                it
            }
            "item/permissions/requestApproval" => {
                let mut it = approval(
                    native,
                    "Permissions",
                    p,
                    Some("Codex asks for more permissions"),
                );
                it.body_md = reason.map(str::to_string);
                it
            }
            "item/tool/requestUserInput" => {
                let questions: Vec<Question> = p
                    .get("questions")
                    .and_then(Value::as_array)
                    .map(|qs| {
                        qs.iter()
                            .enumerate()
                            .map(|(i, q)| Question {
                                id: q
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                                    .unwrap_or_else(|| format!("q{i}")),
                                prompt: q
                                    .get("question")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                header: q.get("header").and_then(Value::as_str).map(str::to_string),
                                multi: false,
                                options: q
                                    .get("options")
                                    .and_then(Value::as_array)
                                    .map(|os| {
                                        os.iter()
                                            .map(|o| {
                                                let l = o
                                                    .get("label")
                                                    .and_then(Value::as_str)
                                                    .unwrap_or("")
                                                    .to_string();
                                                QuestionOption {
                                                    id: l.clone(),
                                                    label: l,
                                                    description: o
                                                        .get("description")
                                                        .and_then(Value::as_str)
                                                        .map(str::to_string),
                                                }
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                                allow_free_text: true,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let title = questions
                    .first()
                    .map(|q| q.prompt.clone())
                    .unwrap_or_else(|| "Codex has a question".into());
                interaction(InteractionKind::Question, native, &title, None, questions)
            }
            "mcpServer/elicitation/request" => {
                let msg = p
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("An MCP server asks for input");
                interaction(
                    InteractionKind::Question,
                    native,
                    msg,
                    None,
                    vec![Question {
                        id: "value".into(),
                        prompt: msg.to_string(),
                        header: None,
                        multi: false,
                        options: vec![],
                        allow_free_text: true,
                    }],
                )
            }
            _ => return None,
        })
    }
}

impl Adapter for Codex {
    fn start(&mut self, cx: &mut Cx) {
        self.request(
            cx,
            "initialize",
            json!({"clientInfo": {"name": "vibeke", "title": "Vibeke", "version": vk_proto::VERSION}}),
        );
    }

    fn on_frame(&mut self, cx: &mut Cx, v: &Value) {
        let method = v.get("method").and_then(Value::as_str);
        let id = v.get("id");
        match (method, id) {
            (None, Some(id)) => {
                let Some(m) = id.as_u64().and_then(|i| self.inflight.remove(&i)) else {
                    return;
                };
                if let Some(e) = v.get("error") {
                    let msg = e.get("message").and_then(Value::as_str).unwrap_or("error");
                    cx.render(format!("! {m}: {msg}\n"));
                    if m == "turn/start" {
                        self.busy = false;
                        cx.signal(
                            "StopFailure",
                            json!({"error_type": "error", "message": msg}),
                        );
                    } else if m == "thread/resume" && self.thread.is_none() {
                        // The thread cannot be resumed: continue in a new one.
                        self.resume = None;
                        self.open_thread(cx);
                    }
                    return;
                }
                let r = v.get("result").cloned().unwrap_or(Value::Null);
                match m.as_str() {
                    "initialize" => {
                        self.initialized = true;
                        cx.write(json!({"method": "initialized"}));
                        if self.thread.is_none() {
                            self.open_thread(cx);
                        }
                    }
                    "thread/start" | "thread/resume" => {
                        if let Some(t) = r.pointer("/thread/id").and_then(Value::as_str) {
                            self.thread = Some(t.to_string());
                            cx.session(t);
                            if !self.identified {
                                self.identified = true;
                                cx.render(format!("thread {t}\n› "));
                                cx.signal(
                                    "SessionStart",
                                    json!({"session_id": t, "cwd": self.cwd, "model": r.get("model"), "source": if m == "thread/resume" { "resume" } else { "startup" }}),
                                );
                            }
                        }
                    }
                    "turn/start" => {
                        if let Some(t) = r.pointer("/turn/id").and_then(Value::as_str) {
                            self.turn = Some(t.to_string());
                        }
                    }
                    "thread/read" => {
                        let status = r
                            .pointer("/thread/status/type")
                            .or_else(|| r.pointer("/thread/status"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if self.busy && matches!(status, "idle" | "notLoaded" | "systemError") {
                            // The turn ended while the journal could not show it (ring overflow).
                            self.busy = false;
                            self.turn = None;
                            cx.render("[done (reconciled)]\n› ");
                            cx.signal(
                                "Stop",
                                json!({"session_id": self.thread, "last_assistant_message": self.last_msg}),
                            );
                        }
                        if status == "notLoaded"
                            && let Some(t) = self.thread.clone()
                        {
                            self.request(cx, "thread/resume", json!({"threadId": t}));
                        }
                    }
                    _ => {}
                }
            }
            (Some(m), Some(id)) => {
                let p = v.get("params").cloned().unwrap_or(Value::Null);
                match self.interaction(id, m, &p) {
                    Some(it) => {
                        if !self.pending.iter().any(|(i, _, _)| i == id) {
                            self.pending.push((id.clone(), m.to_string(), p));
                        }
                        cx.open(it);
                    }
                    None => {
                        self.auto.push(id.clone());
                        cx.write(json!({"id": id, "error": {"code": -32601, "message": "not supported by vibeke"}}));
                    }
                }
            }
            (Some(m), None) => {
                let p = v.get("params").cloned().unwrap_or(Value::Null);
                match m {
                    "thread/started" => {
                        if let Some(t) = p.pointer("/thread/id").and_then(Value::as_str)
                            && self.thread.is_none()
                        {
                            self.thread = Some(t.to_string());
                            cx.session(t);
                        }
                    }
                    "turn/started" => {
                        if let Some(t) = p.pointer("/turn/id").and_then(Value::as_str) {
                            self.turn = Some(t.to_string());
                        }
                        self.busy = true;
                    }
                    "item/started" => {
                        let item = p.get("item").cloned().unwrap_or(Value::Null);
                        let iid = item
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if let Some((tool, input)) = Self::tool_of(&item) {
                            cx.tool_start(&iid, harness::tool_summary(&tool, &input));
                            cx.signal(
                                "PreToolUse",
                                json!({"tool_name": tool, "tool_input": input, "tool_use_id": iid, "session_id": self.thread}),
                            );
                            self.items.insert(iid, (tool, input));
                        }
                    }
                    "item/completed" => {
                        let item = p.get("item").cloned().unwrap_or(Value::Null);
                        let iid = item
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        match item.get("type").and_then(Value::as_str) {
                            Some("agentMessage") => {
                                let t = item.get("text").and_then(Value::as_str).unwrap_or("");
                                self.last_msg = Some(t.to_string());
                                cx.render(format!("{t}\n"));
                            }
                            _ => {
                                let known =
                                    self.items.remove(&iid).or_else(|| Self::tool_of(&item));
                                if let Some((tool, input)) = known {
                                    let status = item
                                        .get("status")
                                        .and_then(Value::as_str)
                                        .unwrap_or("completed");
                                    let failed = matches!(status, "failed" | "declined")
                                        || item
                                            .get("exitCode")
                                            .and_then(Value::as_i64)
                                            .is_some_and(|c| c != 0);
                                    let status = match status {
                                        "declined" => ToolStatus::Declined,
                                        _ if failed => ToolStatus::Failed,
                                        _ => ToolStatus::Done,
                                    };
                                    cx.tool_end(
                                        &iid,
                                        status,
                                        item.get("aggregatedOutput")
                                            .and_then(Value::as_str)
                                            .map(str::to_string),
                                        Self::item_diff(&item),
                                        item.get("exitCode").and_then(Value::as_i64),
                                    );
                                    cx.signal(
                                        if failed { "PostToolUseFailure" } else { "PostToolUse" },
                                        json!({"tool_name": tool, "tool_input": input, "tool_use_id": iid, "exit_code": item.get("exitCode")}),
                                    );
                                }
                            }
                        }
                    }
                    "turn/completed" => {
                        self.busy = false;
                        self.turn = None;
                        let status = p
                            .pointer("/turn/status")
                            .and_then(Value::as_str)
                            .unwrap_or("completed");
                        match status {
                            "failed" => {
                                let msg = p
                                    .pointer("/turn/error/message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("turn failed");
                                let kind = if usage::looks_rate_limited(msg) {
                                    "rate_limit"
                                } else {
                                    "error"
                                };
                                cx.render(format!("! {msg}\n"));
                                cx.signal(
                                    "StopFailure",
                                    json!({"error_type": kind, "message": msg}),
                                );
                            }
                            s => {
                                cx.signal(
                                    if s == "interrupted" { "Interrupt" } else { "Stop" },
                                    json!({"session_id": self.thread, "last_assistant_message": self.last_msg}),
                                );
                            }
                        }
                        cx.render(format!("[{status}]\n› "));
                    }
                    "thread/tokenUsage/updated" => {
                        let t = p
                            .pointer("/tokenUsage/total")
                            .cloned()
                            .unwrap_or(Value::Null);
                        let n = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
                        cx.usage(
                            RunUsage {
                                input_tokens: n("inputTokens"),
                                output_tokens: n("outputTokens") + n("reasoningOutputTokens"),
                                cache_read_tokens: n("cachedInputTokens"),
                                cache_write_tokens: 0,
                                cost_usd: None,
                                model: None,
                                source: "app-server".into(),
                                updated_at_ms: 0,
                            },
                            true,
                        );
                    }
                    "account/rateLimits/updated" => {
                        if let Some(l) = usage::app_server_rate_limit(&p, now_ms()) {
                            if l.limited && !self.limited {
                                cx.render(format!(
                                    "⚠ rate limited ({} window at {:.0}%)\n",
                                    l.scope.as_deref().unwrap_or("usage"),
                                    l.used_percent.unwrap_or(100.0)
                                ));
                            }
                            self.limited = l.limited;
                            cx.rate_limit(l);
                        }
                    }
                    "serverRequest/resolved" => {
                        if let Some(rid) = p.get("requestId") {
                            self.pending.retain(|(i, _, _)| i != rid);
                            cx.resolved(format!("rpc:{}", id_str(rid)));
                        }
                    }
                    "error" => {
                        let msg = p
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .unwrap_or("error");
                        cx.render(format!("! {msg}\n"));
                    }
                    _ => {}
                }
            }
            (None, None) => {}
        }
    }

    fn on_sent(&mut self, cx: &mut Cx, v: &Value) {
        let method = v.get("method").and_then(Value::as_str);
        let id = v.get("id");
        match (method, id) {
            (Some(m), Some(id)) => {
                if let Some(n) = id.as_u64() {
                    self.inflight.insert(n, m.to_string());
                    self.next = self.next.max(n + 1);
                }
                let text = v
                    .pointer("/params/input")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|i| i.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                match m {
                    "turn/start" => {
                        self.busy = true;
                        cx.render(format!("› {text}\n"));
                        cx.signal(
                            "UserPromptSubmit",
                            json!({"prompt": text, "session_id": self.thread}),
                        );
                    }
                    "turn/steer" => cx.render(format!("› (steer) {text}\n")),
                    _ => {}
                }
            }
            (None, Some(id)) => {
                self.pending.retain(|(i, _, _)| i != id);
                self.auto.retain(|i| i != id);
            }
            _ => {}
        }
    }

    fn prompt(&mut self, cx: &mut Cx, text: &str, mode: PromptMode) -> Result<(), String> {
        let Some(thread) = self.thread.clone() else {
            return Err("the Codex thread is not open yet".into());
        };
        let input = json!([{"type": "text", "text": text}]);
        if self.busy {
            match (mode, self.turn.clone()) {
                // Follow-ups wait in the session's durable queue (`Record::queued`), which
                // sends them as new turns once this one ends.
                (PromptMode::FollowUp, _) => {
                    return Err(
                        "a turn is in progress; the follow-up is queued by the session".into(),
                    );
                }
                (_, Some(turn)) => {
                    let id = self.next;
                    self.next += 1;
                    cx.write_prompt(
                        json!({"id": id, "method": "turn/steer", "params": {"threadId": thread, "input": input, "expectedTurnId": turn}}),
                        text,
                    );
                    return Ok(());
                }
                _ => {}
            }
        }
        let id = self.next;
        self.next += 1;
        cx.write_prompt(
            json!({"id": id, "method": "turn/start", "params": {"threadId": thread, "input": input}}),
            text,
        );
        Ok(())
    }

    fn interrupt(&mut self, cx: &mut Cx) {
        if let (Some(t), Some(turn)) = (self.thread.clone(), self.turn.clone()) {
            self.request(cx, "turn/interrupt", json!({"threadId": t, "turnId": turn}));
        }
    }

    fn answer(&mut self, cx: &mut Cx, native_ref: &str, it: &Interaction, a: &Answer) -> bool {
        let Some((id, method, p)) = self
            .pending
            .iter()
            .find(|(i, _, _)| format!("rpc:{}", id_str(i)) == native_ref)
            .cloned()
        else {
            return false;
        };
        let legacy = matches!(
            method.as_str(),
            "execCommandApproval" | "applyPatchApproval"
        );
        let result = match method.as_str() {
            "item/tool/requestUserInput" => {
                let mut answers = serde_json::Map::new();
                for q in &it.questions {
                    if let Some(c) = choice(it, a, &q.id) {
                        answers.insert(q.id.clone(), json!({"answers": [c]}));
                    }
                }
                json!({"answers": answers})
            }
            "mcpServer/elicitation/request" => match (decision(a), choice(it, a, "value")) {
                (Some(Decision::Deny), _) => json!({"action": "decline"}),
                (_, Some(v)) => json!({"action": "accept", "content": {"value": v}}),
                _ => json!({"action": "accept", "content": {}}),
            },
            _ => {
                let d = match (decision(a), legacy) {
                    (Some(Decision::Allow), false) => "accept",
                    (Some(Decision::AllowAlways), false) => "acceptForSession",
                    (_, false) => "decline",
                    (Some(Decision::Allow), true) => "approved",
                    (Some(Decision::AllowAlways), true) => "approved_for_session",
                    (_, true) => "denied",
                };
                let _ = p;
                json!({"decision": d})
            }
        };
        cx.write(json!({"id": id, "result": result}));
        true
    }

    fn pending(&self) -> Vec<Pending> {
        let mut v: Vec<Pending> = self
            .pending
            .iter()
            .map(|(id, m, p)| Pending {
                native_ref: format!("rpc:{}", id_str(id)),
                interaction: self.interaction(id, m, p),
            })
            .collect();
        v.extend(self.auto.iter().map(|id| Pending {
            native_ref: format!("rpc:{}", id_str(id)),
            interaction: None,
        }));
        v
    }

    fn reconcile(&mut self, cx: &mut Cx, _gap: bool) {
        if !self.initialized {
            if !self.inflight_has("initialize") {
                self.start(cx);
            }
        } else if self.thread.is_none() {
            if !self.inflight_has("thread/start") && !self.inflight_has("thread/resume") {
                self.open_thread(cx);
            }
        } else if let Some(t) = self.thread.clone() {
            // Before re-answering anything: what does the thread say (04 §6.2)?
            self.request(
                cx,
                "thread/read",
                json!({"threadId": t, "includeTurns": false}),
            );
        }
        for id in self.auto.clone() {
            cx.write(
                json!({"id": id, "error": {"code": -32601, "message": "not supported by vibeke"}}),
            );
        }
    }

    fn ready(&self) -> bool {
        self.thread.is_some()
    }

    fn busy(&self) -> bool {
        self.busy
    }

    fn native_follow_up(&self) -> bool {
        false
    }
}
