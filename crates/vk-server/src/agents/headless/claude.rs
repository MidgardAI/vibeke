//! Claude Code headless (04 §6.1.3): `claude -p --input-format stream-json --output-format
//! stream-json --verbose --permission-prompt-tool stdio`.
//!
//! | stream-json | Vibeke |
//! |---|---|
//! | (launch with `--session-id`) | `SessionStart` at once: the id is pre-assigned |
//! | `system/init {session_id, model, cwd}` | identity confirmed (re-identify if it differs) |
//! | user message written by Vibeke | `UserPromptSubmit` (turn) |
//! | `assistant` `text` / `tool_use {id, name, input}` | transcript / `PreToolUse` (item) |
//! | `user` `tool_result {tool_use_id, is_error}` | `PostToolUse(Failure)` |
//! | `result {subtype, result, usage, total_cost_usd}` | usage, `Stop` / `StopFailure` |
//! | `control_request {subtype: can_use_tool}` | approval (question for `AskUserQuestion`, plan review for `ExitPlanMode`), answered with `control_response {behavior: allow|deny}` |
//! | `control_cancel_request` | resolved by the harness |
//! | `agent.interrupt` | `control_request {subtype: interrupt}` |
//!
//! The control protocol (`--permission-prompt-tool stdio`) is the one the Agent SDK uses
//! **[verify M0]** against a live `claude`; only fake harnesses exercise it here.

use super::*;

pub struct Claude {
    session: Option<String>,
    resume: bool,
    cwd: String,
    busy: bool,
    turn_text: String,
    next: u64,
    /// tool_use id → (name, input).
    tools: HashMap<String, (String, Value)>,
    /// Unanswered `can_use_tool` requests: (request id, request body).
    pending: Vec<(String, Value)>,
    /// Unanswered requests answered automatically with an error.
    auto: Vec<String>,
}

impl Claude {
    pub fn new(rec: &Record) -> Claude {
        Claude {
            session: rec.session.clone(),
            resume: rec.resume,
            cwd: rec.cwd.clone(),
            busy: false,
            turn_text: String::new(),
            next: 1,
            tools: HashMap::new(),
            pending: vec![],
            auto: vec![],
        }
    }

    fn request_id(&mut self) -> String {
        let id = format!("vk-{}", self.next);
        self.next += 1;
        id
    }

    fn interaction(&self, rid: &str, req: &Value) -> Interaction {
        let tool = req
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let input = req.get("input").cloned().unwrap_or(json!({}));
        let native = format!("rpc:{rid}");
        match tool {
            "AskUserQuestion" => {
                let questions = input
                    .get("questions")
                    .and_then(Value::as_array)
                    .map(|qs| {
                        qs.iter()
                            .enumerate()
                            .map(|(i, q)| Question {
                                id: format!("q{i}"),
                                prompt: q
                                    .get("question")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                header: q.get("header").and_then(Value::as_str).map(str::to_string),
                                multi: q
                                    .get("multiSelect")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false),
                                options: q
                                    .get("options")
                                    .and_then(Value::as_array)
                                    .map(|os| {
                                        os.iter()
                                            .map(|o| {
                                                let label = o
                                                    .get("label")
                                                    .and_then(Value::as_str)
                                                    .unwrap_or("")
                                                    .to_string();
                                                QuestionOption {
                                                    id: label.clone(),
                                                    label,
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
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let title = questions
                    .first()
                    .map(|q| q.prompt.clone())
                    .unwrap_or_else(|| "Claude has a question".into());
                interaction(InteractionKind::Question, native, &title, None, questions)
            }
            "ExitPlanMode" => {
                let mut it = interaction(
                    InteractionKind::PlanReview,
                    native,
                    "Claude wants to leave plan mode",
                    None,
                    vec![],
                );
                it.plan_md = input
                    .get("plan")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                it
            }
            _ => approval(native, tool, &input, None),
        }
    }
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

impl Adapter for Claude {
    fn start(&mut self, cx: &mut Cx) {
        let id = self.request_id();
        cx.write(json!({"type": "control_request", "request_id": id, "request": {"subtype": "initialize", "hooks": null}}));
        if let Some(s) = self.session.clone() {
            // Pre-assigned (`--session-id`) or resumed: identified from second zero (04 §3.3).
            cx.signal(
                "SessionStart",
                json!({"session_id": s, "cwd": self.cwd, "source": if self.resume { "resume" } else { "startup" }}),
            );
        }
    }

    fn on_frame(&mut self, cx: &mut Cx, v: &Value) {
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "system" if v.get("subtype").and_then(Value::as_str) == Some("init") => {
                if let Some(s) = v.get("session_id").and_then(Value::as_str) {
                    cx.session(s);
                    if self.session.as_deref() != Some(s) {
                        let first = self.session.is_none();
                        self.session = Some(s.to_string());
                        let p = json!({"session_id": s, "cwd": v.get("cwd"), "model": v.get("model"), "permission_mode": v.get("permissionMode")});
                        cx.signal(
                            if first && !self.busy {
                                "SessionStart"
                            } else {
                                "Identified"
                            },
                            p,
                        );
                    }
                }
            }
            "system" => {
                if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
                    cx.signal("PostCompact", json!({}));
                }
            }
            "assistant" => {
                let blocks = v
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            let t = b.get("text").and_then(Value::as_str).unwrap_or("");
                            self.turn_text.push_str(t);
                            cx.render(format!("{t}\n"));
                        }
                        Some("tool_use") => {
                            let id = b
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let name = b
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string();
                            let input = b.get("input").cloned().unwrap_or(json!({}));
                            cx.render(format!(
                                "⏺ {name} {}\n",
                                harness::tool_summary(&name, &input)
                            ));
                            cx.signal(
                                "PreToolUse",
                                json!({"tool_name": name, "tool_input": input, "tool_use_id": id, "session_id": self.session}),
                            );
                            self.tools.insert(id, (name, input));
                        }
                        _ => {}
                    }
                }
            }
            "user" => {
                let blocks = v
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let id = b
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let failed = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                    let (name, input) = self
                        .tools
                        .remove(&id)
                        .unwrap_or_else(|| ("tool".into(), json!({})));
                    cx.render(format!("  {} {name}\n", if failed { "✗" } else { "✓" }));
                    cx.signal(
                        if failed {
                            "PostToolUseFailure"
                        } else {
                            "PostToolUse"
                        },
                        json!({"tool_name": name, "tool_input": input, "tool_use_id": id}),
                    );
                }
            }
            "result" => {
                self.busy = false;
                let u = v.get("usage").cloned().unwrap_or(Value::Null);
                let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                cx.usage(
                    RunUsage {
                        input_tokens: n("input_tokens"),
                        output_tokens: n("output_tokens"),
                        cache_read_tokens: n("cache_read_input_tokens"),
                        cache_write_tokens: n("cache_creation_input_tokens"),
                        cost_usd: v.get("total_cost_usd").and_then(Value::as_f64),
                        model: None,
                        source: "stream-json".into(),
                        updated_at_ms: 0,
                    },
                    false,
                );
                let subtype = v
                    .get("subtype")
                    .and_then(Value::as_str)
                    .unwrap_or("success");
                let text = v
                    .get("result")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| std::mem::take(&mut self.turn_text));
                self.turn_text.clear();
                if v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                    || subtype != "success"
                {
                    let kind = if usage::looks_rate_limited(&text) {
                        "rate_limit".to_string()
                    } else {
                        subtype.to_string()
                    };
                    cx.render(format!("! {subtype}: {text}\n"));
                    cx.signal("StopFailure", json!({"error_type": kind, "message": text}));
                } else {
                    cx.signal(
                        "Stop",
                        json!({"session_id": self.session, "last_assistant_message": text}),
                    );
                }
                cx.render("[done]\n› ");
            }
            "control_request" => {
                let rid = v
                    .get("request_id")
                    .map(|r| {
                        r.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| r.to_string())
                    })
                    .unwrap_or_default();
                let req = v.get("request").cloned().unwrap_or(Value::Null);
                if req.get("subtype").and_then(Value::as_str) == Some("can_use_tool") {
                    if !self.pending.iter().any(|(r, _)| *r == rid) {
                        self.pending.push((rid.clone(), req.clone()));
                    }
                    cx.open(self.interaction(&rid, &req));
                } else {
                    self.auto.push(rid.clone());
                    cx.write(json!({"type": "control_response", "response": {"subtype": "error", "request_id": rid, "error": "not supported by vibeke"}}));
                }
            }
            "control_cancel_request" => {
                if let Some(rid) = v.get("request_id").and_then(Value::as_str) {
                    self.pending.retain(|(r, _)| r != rid);
                    cx.resolved(format!("rpc:{rid}"));
                }
            }
            _ => {}
        }
    }

    fn on_sent(&mut self, cx: &mut Cx, v: &Value) {
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "user" => {
                let text = text_of(v.pointer("/message/content").unwrap_or(&Value::Null));
                self.busy = true;
                self.turn_text.clear();
                cx.render(format!("› {text}\n"));
                cx.signal(
                    "UserPromptSubmit",
                    json!({"prompt": text, "session_id": self.session}),
                );
            }
            "control_response" => {
                if let Some(rid) = v.pointer("/response/request_id").and_then(Value::as_str) {
                    self.pending.retain(|(r, _)| r != rid);
                    self.auto.retain(|r| r != rid);
                }
            }
            "control_request" => {
                if let Some(n) = v
                    .get("request_id")
                    .and_then(Value::as_str)
                    .and_then(|r| r.strip_prefix("vk-"))
                    .and_then(|n| n.parse::<u64>().ok())
                {
                    self.next = self.next.max(n + 1);
                }
            }
            _ => {}
        }
    }

    fn prompt(&mut self, cx: &mut Cx, text: &str, _mode: PromptMode) -> Result<(), String> {
        // stream-json queues a user message sent mid-turn: steer and follow-up are the same.
        cx.write_prompt(
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}, "parent_tool_use_id": null, "session_id": self.session.clone().unwrap_or_default()}),
            text,
        );
        Ok(())
    }

    fn interrupt(&mut self, cx: &mut Cx) {
        let id = self.request_id();
        cx.write(json!({"type": "control_request", "request_id": id, "request": {"subtype": "interrupt"}}));
    }

    fn answer(&mut self, cx: &mut Cx, native_ref: &str, it: &Interaction, a: &Answer) -> bool {
        let Some(rid) = native_ref.strip_prefix("rpc:") else {
            return false;
        };
        let Some((_, req)) = self.pending.iter().find(|(r, _)| r == rid).cloned() else {
            return false;
        };
        let tool = req.get("tool_name").and_then(Value::as_str).unwrap_or("");
        let mut input = req.get("input").cloned().unwrap_or(json!({}));
        let response = match (tool, decision(a)) {
            ("AskUserQuestion", d) if d != Some(Decision::Deny) => {
                let mut answers = serde_json::Map::new();
                for q in &it.questions {
                    if let Some(c) = choice(it, a, &q.id) {
                        answers.insert(q.prompt.clone(), json!(c));
                    }
                }
                if let Some(o) = input.as_object_mut() {
                    o.insert("answers".into(), Value::Object(answers));
                }
                json!({"behavior": "allow", "updatedInput": input})
            }
            (_, Some(Decision::Allow)) => json!({"behavior": "allow", "updatedInput": input}),
            (_, Some(Decision::AllowAlways)) => {
                let mut r = json!({"behavior": "allow", "updatedInput": input});
                if let Some(s) = req.get("permission_suggestions").filter(|s| !s.is_null()) {
                    r["updatedPermissions"] = s.clone();
                }
                r
            }
            _ => {
                json!({"behavior": "deny", "message": a.text.clone().unwrap_or_else(|| "Denied by the user in Vibeke".into())})
            }
        };
        cx.write(json!({"type": "control_response", "response": {"subtype": "success", "request_id": rid, "response": response}}));
        true
    }

    fn pending(&self) -> Vec<Pending> {
        let mut v: Vec<Pending> = self
            .pending
            .iter()
            .map(|(rid, req)| Pending {
                native_ref: format!("rpc:{rid}"),
                interaction: Some(self.interaction(rid, req)),
            })
            .collect();
        v.extend(self.auto.iter().map(|r| Pending {
            native_ref: format!("rpc:{r}"),
            interaction: None,
        }));
        v
    }

    fn reconcile(&mut self, cx: &mut Cx, _gap: bool) {
        // stream-json has no state query: the journal is the source of truth. Requests the
        // journal shows unanswered and that Vibeke refuses are refused now.
        for rid in self.auto.clone() {
            cx.write(json!({"type": "control_response", "response": {"subtype": "error", "request_id": rid, "error": "not supported by vibeke"}}));
        }
    }

    fn ready(&self) -> bool {
        true
    }

    fn busy(&self) -> bool {
        self.busy
    }
}
