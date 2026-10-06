//! pi / omp headless (04 §6.3): `pi --mode rpc` / `omp --mode rpc`, JSONL split on `\n` only
//! (pi `rpc.md` §Framing; never a readline-style splitter that breaks on U+2028/U+2029).
//!
//! | RPC | Vibeke |
//! |---|---|
//! | `get_state` response `{sessionId, sessionFile, isStreaming}` | `SessionStart`; reconcile after a restart |
//! | `prompt` written by Vibeke (`steer` / `follow_up` mid-turn) | `UserPromptSubmit` (turn) |
//! | `tool_execution_start` / `_end` | `PreToolUse` / `PostToolUse(Failure)` |
//! | `message_end` (assistant) | transcript + last message |
//! | `turn_end.message.usage` | usage |
//! | `agent_end` | `Stop` |
//! | `compaction_start` / `_end` | `PreCompact` / `PostCompact` |
//! | `extension_ui_request` `confirm` / `select` / `input` / `editor` | approval / question, answered with `extension_ui_response` |
//! | `agent.interrupt` | `abort` |
//!
//! There is no Vibeke approval gate for pi (04 §6.3): what is answered here are the dialogs a
//! user's own permission extension opens.

use super::*;

pub struct Pi {
    cwd: String,
    ready: bool,
    busy: bool,
    session: Option<String>,
    next: u64,
    /// Our commands in flight: id → command type.
    inflight: HashMap<String, String>,
    /// Unanswered dialogs: (id, request).
    pending: Vec<(String, Value)>,
    /// toolCallId → (tool, args).
    tools: HashMap<String, (String, Value)>,
    last_msg: Option<String>,
}

fn tool_name(t: &str) -> String {
    match t {
        "bash" => "Bash".into(),
        "edit" => "Edit".into(),
        "write" => "Write".into(),
        "read" => "Read".into(),
        "grep" => "Grep".into(),
        "find" | "ls" => "Glob".into(),
        other => other.to_string(),
    }
}

impl Pi {
    pub fn new(rec: &Record) -> Pi {
        Pi {
            cwd: rec.cwd.clone(),
            // Rebuilt after a restart: the session was reported before.
            ready: rec.established(),
            busy: false,
            session: rec.session.clone(),
            next: 1,
            inflight: HashMap::new(),
            pending: vec![],
            tools: HashMap::new(),
            last_msg: None,
        }
    }

    fn command(&mut self, cx: &mut Cx, ty: &str, extra: Value) {
        let id = format!("vk-{}", self.next);
        self.next += 1;
        let mut v = json!({"id": id, "type": ty});
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            for (k, x) in e {
                o.insert(k.clone(), x.clone());
            }
        }
        cx.write(v);
    }

    fn interaction(&self, id: &str, req: &Value) -> Option<Interaction> {
        let method = req.get("method").and_then(Value::as_str)?;
        let title = req
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("pi extension dialog");
        let message = req.get("message").and_then(Value::as_str);
        let native = format!("rpc:{id}");
        let mut it = match method {
            "confirm" => {
                let mut it = interaction(InteractionKind::Approval, native, title, None, vec![]);
                it.action = Some(ActionInfo {
                    tool: "extension".into(),
                    summary: message.unwrap_or(title).to_string(),
                    command: None,
                    paths: vec![],
                    diff: None,
                    risk: Risk::Unknown,
                    risk_reasons: vec![],
                });
                it
            }
            "select" => {
                let options = req
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|os| {
                        os.iter()
                            .filter_map(|o| {
                                o.as_str().map(str::to_string).or_else(|| {
                                    o.get("label").and_then(Value::as_str).map(str::to_string)
                                })
                            })
                            .map(|l| QuestionOption {
                                id: l.clone(),
                                label: l,
                                description: None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                interaction(
                    InteractionKind::Question,
                    native,
                    title,
                    None,
                    vec![Question {
                        id: "choice".into(),
                        prompt: title.to_string(),
                        header: None,
                        multi: false,
                        options,
                        allow_free_text: false,
                    }],
                )
            }
            "input" | "editor" => interaction(
                InteractionKind::Question,
                native,
                title,
                None,
                vec![Question {
                    id: "value".into(),
                    prompt: message.unwrap_or(title).to_string(),
                    header: None,
                    multi: false,
                    options: vec![],
                    allow_free_text: true,
                }],
            ),
            _ => return None,
        };
        it.body_md = message.map(str::to_string);
        Some(it)
    }
}

impl Adapter for Pi {
    fn start(&mut self, cx: &mut Cx) {
        self.command(cx, "get_state", json!({}));
    }

    fn on_frame(&mut self, cx: &mut Cx, v: &Value) {
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "response" => {
                let id = v.get("id").and_then(Value::as_str).unwrap_or("");
                let cmd = self.inflight.remove(id).unwrap_or_default();
                if v.get("success").and_then(Value::as_bool) == Some(false) {
                    let e = v.get("error").and_then(Value::as_str).unwrap_or("error");
                    cx.render(format!("! {cmd}: {e}\n"));
                    if cmd == "prompt" {
                        self.busy = false;
                        cx.signal("StopFailure", json!({"error_type": "error", "message": e}));
                    }
                    return;
                }
                if cmd == "get_state" {
                    let d = v.get("data").cloned().unwrap_or(Value::Null);
                    let sid = d
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    if let Some(s) = &sid {
                        cx.session(s);
                    }
                    if !self.ready {
                        self.ready = true;
                        self.session = sid.clone().or(self.session.take());
                        cx.render(format!(
                            "session {}\n› ",
                            self.session.as_deref().unwrap_or("?")
                        ));
                        cx.signal(
                            "SessionStart",
                            json!({"session_id": self.session, "transcript_path": d.get("sessionFile"), "model": d.pointer("/model/id").or(d.get("model")), "cwd": self.cwd}),
                        );
                    } else if self.busy
                        && d.get("isStreaming").and_then(Value::as_bool) == Some(false)
                    {
                        // Reconcile: the run settled while nobody watched (or the ring lost it).
                        self.busy = false;
                        cx.render("[done (reconciled)]\n› ");
                        cx.signal(
                            "Stop",
                            json!({"session_id": self.session, "last_assistant_message": self.last_msg}),
                        );
                    }
                }
            }
            "agent_start" => self.busy = true,
            "message_end" => {
                let m = v.get("message").cloned().unwrap_or(Value::Null);
                if m.get("role").and_then(Value::as_str) == Some("assistant") {
                    let t: String = m
                        .get("content")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                                .filter_map(|b| b.get("text").and_then(Value::as_str))
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .unwrap_or_default();
                    if !t.is_empty() {
                        self.last_msg = Some(t.clone());
                        cx.render(format!("{t}\n"));
                    }
                }
            }
            "turn_end" => {
                if let Some(u) = v.pointer("/message/usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    cx.usage(
                        RunUsage {
                            input_tokens: n("input"),
                            output_tokens: n("output"),
                            cache_read_tokens: n("cacheRead"),
                            cache_write_tokens: n("cacheWrite"),
                            cost_usd: u.pointer("/cost/total").and_then(Value::as_f64),
                            model: None,
                            source: "rpc".into(),
                            updated_at_ms: 0,
                        },
                        false,
                    );
                }
            }
            "tool_execution_start" => {
                let id = v
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let tool = tool_name(v.get("toolName").and_then(Value::as_str).unwrap_or("tool"));
                let mut input = v.get("args").cloned().unwrap_or(json!({}));
                if let (Some(o), Some(p)) = (
                    input.as_object_mut(),
                    v.pointer("/args/path")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                ) {
                    o.entry("file_path").or_insert(json!(p));
                }
                cx.render(format!(
                    "⏺ {tool} {}\n",
                    harness::tool_summary(&tool, &input)
                ));
                cx.signal(
                    "PreToolUse",
                    json!({"tool_name": tool, "tool_input": input, "tool_use_id": id}),
                );
                self.tools.insert(id, (tool, input));
            }
            "tool_execution_end" => {
                let id = v
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let failed = v.get("isError").and_then(Value::as_bool).unwrap_or(false);
                let (tool, input) = self.tools.remove(&id).unwrap_or_else(|| {
                    (
                        tool_name(v.get("toolName").and_then(Value::as_str).unwrap_or("tool")),
                        json!({}),
                    )
                });
                cx.render(format!("  {} {tool}\n", if failed { "✗" } else { "✓" }));
                cx.signal(
                    if failed {
                        "PostToolUseFailure"
                    } else {
                        "PostToolUse"
                    },
                    json!({"tool_name": tool, "tool_input": input, "tool_use_id": id}),
                );
            }
            "agent_end" => {
                self.busy = false;
                cx.render("[done]\n› ");
                cx.signal(
                    "Stop",
                    json!({"session_id": self.session, "last_assistant_message": self.last_msg}),
                );
            }
            "compaction_start" | "auto_compaction_start" => cx.signal("PreCompact", json!({})),
            "compaction_end" | "auto_compaction_end" => cx.signal("PostCompact", json!({})),
            "auto_retry_start" => {
                let m = v
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .unwrap_or("retrying");
                cx.render(format!("\x1b[2m(retrying: {m})\x1b[0m\n"));
            }
            "extension_ui_request" => {
                let id = v
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                match self.interaction(&id, v) {
                    Some(it) => {
                        if !self.pending.iter().any(|(i, _)| *i == id) {
                            self.pending.push((id, v.clone()));
                        }
                        cx.open(it);
                    }
                    None => {
                        // Fire-and-forget methods (notify, setStatus, setWidget, setTitle, …).
                        if v.get("method").and_then(Value::as_str) == Some("notify")
                            && let Some(m) = v.get("message").and_then(Value::as_str)
                        {
                            cx.render(format!("ℹ {m}\n"));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn on_sent(&mut self, cx: &mut Cx, v: &Value) {
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        if let Some(id) = v.get("id").and_then(Value::as_str) {
            if ty == "extension_ui_response" {
                self.pending.retain(|(i, _)| i != id);
                return;
            }
            self.inflight.insert(id.to_string(), ty.to_string());
            if let Some(n) = id.strip_prefix("vk-").and_then(|n| n.parse::<u64>().ok()) {
                self.next = self.next.max(n + 1);
            }
        }
        let text = v.get("message").and_then(Value::as_str).unwrap_or("");
        match ty {
            "prompt" => {
                self.busy = true;
                cx.render(format!("› {text}\n"));
                cx.signal(
                    "UserPromptSubmit",
                    json!({"prompt": text, "session_id": self.session}),
                );
            }
            "steer" | "follow_up" => cx.render(format!("› ({ty}) {text}\n")),
            _ => {}
        }
    }

    fn prompt(&mut self, cx: &mut Cx, text: &str, mode: PromptMode) -> Result<(), String> {
        let ty = match (self.busy, mode) {
            (false, _) => "prompt",
            (true, PromptMode::Steer) => "steer",
            (true, _) => "follow_up",
        };
        let id = format!("vk-{}", self.next);
        self.next += 1;
        cx.write_prompt(json!({"id": id, "type": ty, "message": text}), text);
        Ok(())
    }

    fn interrupt(&mut self, cx: &mut Cx) {
        self.command(cx, "abort", json!({}));
    }

    fn answer(&mut self, cx: &mut Cx, native_ref: &str, it: &Interaction, a: &Answer) -> bool {
        let Some(id) = native_ref.strip_prefix("rpc:") else {
            return false;
        };
        let Some((_, req)) = self.pending.iter().find(|(i, _)| i == id).cloned() else {
            return false;
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let mut r = json!({"type": "extension_ui_response", "id": id});
        match method {
            "confirm" => {
                r["confirmed"] = json!(matches!(
                    decision(a),
                    Some(Decision::Allow | Decision::AllowAlways)
                ));
            }
            _ => match (
                decision(a),
                it.questions.first().and_then(|q| choice(it, a, &q.id)),
            ) {
                (Some(Decision::Deny), _) | (_, None) => r["cancelled"] = json!(true),
                (_, Some(v)) => r["value"] = json!(v),
            },
        }
        cx.write(r);
        true
    }

    fn pending(&self) -> Vec<Pending> {
        self.pending
            .iter()
            .map(|(id, req)| Pending {
                native_ref: format!("rpc:{id}"),
                interaction: self.interaction(id, req),
            })
            .collect()
    }

    fn reconcile(&mut self, cx: &mut Cx, _gap: bool) {
        // `get_state` both finishes an interrupted start and catches a settled run.
        if !self.inflight.values().any(|c| c == "get_state") {
            self.command(cx, "get_state", json!({}));
        }
    }

    fn ready(&self) -> bool {
        self.ready
    }

    fn busy(&self) -> bool {
        self.busy
    }
}
