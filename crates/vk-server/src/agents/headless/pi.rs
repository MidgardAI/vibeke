//! pi / omp headless (04 §6.3): `pi --mode rpc` / `omp --mode rpc-ui`, JSONL split on `\n` only
//! (pi `rpc.md` §Framing; never a readline-style splitter that breaks on U+2028/U+2029).
//!
//! | RPC | Vibeke |
//! |---|---|
//! | `get_state` response `{sessionId, sessionFile, isStreaming}` | `SessionStart`; reconcile after a restart |
//! | `prompt` written by Vibeke (`steer` / `follow_up` mid-turn) | `UserPromptSubmit` (turn) |
//! | `tool_execution_start` / `_end` | `PreToolUse` / `PostToolUse(Failure)` |
//! | `message_end` (assistant) | transcript + last message |
//! | `turn_end.message.usage` | usage (per turn) |
//! | `agent_end` | `Stop`, then `get_session_stats` |
//! | `get_session_stats` response `{tokens {input, output, cacheRead, cacheWrite}, cost}` | usage (session total) |
//! | `compaction_start` / `_end` | `PreCompact` / `PostCompact` |
//! | `extension_ui_request` `confirm` / `select` / `input` / `editor` | approval / question, answered with `extension_ui_response` |
//! | `agent.interrupt` | `abort` |
//! | `agent.models` | `get_available_models` (current: `get_state` / `set_model` model) |
//! | `agent.set_model` | `set_model {provider, modelId}` (pi also saves it as its default model; omp keeps it to the session) |
//! | omp `tool_approval_requested {toolCallId, toolName, reason}` / `_resolved {approved}` | binds the approval dialog to the tool call; resolved by the harness |
//!
//! There is no Vibeke approval gate for pi (04 §6.3): what is answered here are the dialogs a
//! user's own permission extension opens. omp runs in `rpc-ui` mode, which also carries omp's
//! own tool approvals (**[verify M0]** against a live omp): a `confirm`/`select` dialog that
//! names a `toolCallId`, or follows a `tool_approval_requested` not yet matched to a dialog,
//! becomes an *approval* for that tool call (risk-scored, so policy rules apply), answered
//! `confirmed` or with the option that reads as allow / allow always / deny.

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
    /// omp (`rpc-ui`): tool approvals arrive as dialogs.
    omp: bool,
    /// omp approvals requested and not yet bound to a dialog: (toolCallId, tool, input).
    approvals: Vec<(String, String, Value)>,
    /// Dialog id → (toolCallId, tool, input) it approves.
    dialog_tool: HashMap<String, (String, String, Value)>,
    /// Current model as `provider/id`.
    model: Option<String>,
}

/// `provider/id` of an RPC `Model` object.
fn model_key(m: &Value) -> Option<String> {
    let id = m.get("id").and_then(Value::as_str)?;
    Some(match m.get("provider").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => format!("{p}/{id}"),
        _ => id.to_string(),
    })
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
            omp: Harness::from_id(&rec.harness).is_some_and(|h| h.base() == Harness::Omp),
            approvals: vec![],
            dialog_tool: HashMap::new(),
            model: None,
        }
    }

    /// Write a command and return its id.
    fn command_id(&mut self, cx: &mut Cx, ty: &str, extra: Value) -> String {
        let id = format!("vk-{}", self.next);
        self.command(cx, ty, extra);
        id
    }

    /// The tool call an omp dialog approves: named by `toolCallId`, else the oldest approval
    /// request not yet matched to a dialog.
    fn bind_approval(&mut self, id: &str, req: &Value) {
        if !self.omp || self.dialog_tool.contains_key(id) {
            return;
        }
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        if !matches!(method, "confirm" | "select") {
            return;
        }
        let named = req
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let pos = match &named {
            Some(t) => self.approvals.iter().position(|a| &a.0 == t),
            None => (!self.approvals.is_empty()).then_some(0),
        };
        let bound = match (pos, named) {
            (Some(i), _) => self.approvals.remove(i),
            (None, Some(t)) => {
                let (tool, input) = self.tools.get(&t).cloned().unwrap_or_else(|| {
                    (
                        tool_name(
                            req.get("toolName")
                                .and_then(Value::as_str)
                                .unwrap_or("tool"),
                        ),
                        req.get("args").cloned().unwrap_or(json!({})),
                    )
                });
                (t, tool, input)
            }
            (None, None) => return,
        };
        self.dialog_tool.insert(id.to_string(), bound);
    }

    /// The option of an approval `select` that reads as decision `d`.
    fn approval_option(req: &Value, d: Option<Decision>) -> Option<String> {
        let labels: Vec<String> = req
            .get("options")
            .and_then(Value::as_array)?
            .iter()
            .filter_map(|o| {
                o.as_str()
                    .map(str::to_string)
                    .or_else(|| o.get("label").and_then(Value::as_str).map(str::to_string))
            })
            .collect();
        let has = |l: &str, ks: &[&str]| {
            let l = l.to_lowercase();
            ks.iter()
                .any(|k| l.split(|c: char| !c.is_alphanumeric()).any(|w| w == *k))
        };
        let always = ["always", "session", "remember", "forever"];
        let deny = [
            "deny", "reject", "no", "block", "cancel", "decline", "never",
        ];
        let pick = match d {
            Some(Decision::AllowAlways) => labels
                .iter()
                .find(|l| has(l, &always) && !has(l, &deny))
                .or_else(|| labels.iter().find(|l| !has(l, &deny))),
            Some(Decision::Allow) => labels.iter().find(|l| !has(l, &always) && !has(l, &deny)),
            _ => labels.iter().find(|l| has(l, &deny)),
        };
        pick.cloned()
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
        if let Some((_, tool, input)) = self.dialog_tool.get(id) {
            let mut it = approval(native, tool, input, Some(title));
            it.body_md = message.map(str::to_string);
            return Some(it);
        }
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
                let failed = v.get("success").and_then(Value::as_bool) == Some(false);
                if matches!(cmd.as_str(), "get_available_models" | "set_model") {
                    let d = v.get("data").cloned().unwrap_or(Value::Null);
                    let r = if failed {
                        let e = v.get("error").and_then(Value::as_str).unwrap_or("error");
                        Err(format!("{cmd}: {e}"))
                    } else if cmd == "set_model" {
                        self.model = model_key(&d).or(self.model.take());
                        // pi's `set_model` saves the default model too; omp's keeps it to the
                        // session.
                        Ok(json!({"default_changed": !self.omp}))
                    } else {
                        let models: Vec<Value> = d
                            .get("models")
                            .and_then(Value::as_array)
                            .map(|a| {
                                a.iter()
                                    .filter_map(|m| {
                                        let key = model_key(m)?;
                                        let label = m
                                            .get("name")
                                            .and_then(Value::as_str)
                                            .filter(|n| !n.is_empty())
                                            .unwrap_or(&key)
                                            .to_string();
                                        let mut o = json!({"id": key, "label": label, "current": self.model.as_deref() == Some(key.as_str())});
                                        if let Some(p) = m.get("provider").and_then(Value::as_str) {
                                            o["description"] = json!(p);
                                        }
                                        Some(o)
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        Ok(json!({"models": models}))
                    };
                    cx.reply(id.to_string(), r);
                    return;
                }
                if failed {
                    if cmd == "get_session_stats" {
                        // Older pi/omp builds lack it; per-turn usage stays.
                        return;
                    }
                    let e = v.get("error").and_then(Value::as_str).unwrap_or("error");
                    cx.render(format!("! {cmd}: {e}\n"));
                    if cmd == "prompt" {
                        self.busy = false;
                        cx.signal("StopFailure", json!({"error_type": "error", "message": e}));
                    }
                    return;
                }
                if cmd == "get_session_stats" {
                    let d = v.get("data").cloned().unwrap_or(Value::Null);
                    if let Some(t) = d.get("tokens") {
                        let n = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
                        cx.usage(
                            RunUsage {
                                input_tokens: n("input"),
                                output_tokens: n("output"),
                                cache_read_tokens: n("cacheRead"),
                                cache_write_tokens: n("cacheWrite"),
                                cost_usd: d
                                    .get("cost")
                                    .and_then(|c| c.as_f64().or_else(|| c.get("total")?.as_f64())),
                                model: None,
                                source: "rpc".into(),
                                updated_at_ms: 0,
                            },
                            true,
                        );
                    }
                    return;
                }
                if cmd == "get_state" {
                    let d = v.get("data").cloned().unwrap_or(Value::Null);
                    if let Some(m) = d.get("model").and_then(model_key) {
                        self.model = Some(m);
                    }
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
                cx.tool_start(&id, harness::tool_summary(&tool, &input));
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
                let result = v.get("result").cloned().unwrap_or(Value::Null);
                // pi's edit tool reports its own diff (`details.diff`); otherwise derive one.
                let diff = result
                    .pointer("/details/diff")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        (!failed && matches!(tool.as_str(), "Edit" | "Write"))
                            .then(|| transcript::input_diff(&input))
                            .flatten()
                    });
                let output = transcript::result_text(&result).filter(|_| diff.is_none() || failed);
                cx.tool_end(
                    &id,
                    if failed {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Done
                    },
                    output,
                    diff,
                    None,
                );
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
                // The session total replaces the per-turn deltas (04 §6.3).
                self.command(cx, "get_session_stats", json!({}));
            }
            "tool_approval_requested" => {
                let tcid = v
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let tool = tool_name(v.get("toolName").and_then(Value::as_str).unwrap_or("tool"));
                let input = v
                    .get("args")
                    .or_else(|| v.get("input"))
                    .cloned()
                    .or_else(|| self.tools.get(&tcid).map(|(_, i)| i.clone()))
                    .unwrap_or(json!({}));
                let reason = v.get("reason").and_then(Value::as_str);
                cx.render(format!(
                    "⏸ approval requested: {}{}\n",
                    harness::tool_summary(&tool, &input),
                    reason.map(|r| format!(" ({r})")).unwrap_or_default()
                ));
                let bound = self.dialog_tool.values().any(|(t, _, _)| *t == tcid);
                if !bound && !self.approvals.iter().any(|a| a.0 == tcid) {
                    self.approvals.push((tcid, tool, input));
                }
            }
            "tool_approval_resolved" => {
                let tcid = v.get("toolCallId").and_then(Value::as_str).unwrap_or("");
                let approved = v.get("approved").and_then(Value::as_bool).unwrap_or(false);
                self.approvals.retain(|a| a.0 != tcid);
                cx.render(format!(
                    "  approval {}\n",
                    if approved { "granted" } else { "denied" }
                ));
                // Answered elsewhere (omp's own UI or a timeout): the dialog is gone.
                let dialogs: Vec<String> = self
                    .dialog_tool
                    .iter()
                    .filter(|(_, (t, _, _))| t == tcid)
                    .map(|(d, _)| d.clone())
                    .collect();
                for d in dialogs {
                    self.dialog_tool.remove(&d);
                    if self.pending.iter().any(|(i, _)| *i == d) {
                        self.pending.retain(|(i, _)| *i != d);
                        cx.resolved(format!("rpc:{d}"));
                    }
                }
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
                self.bind_approval(&id, v);
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
                self.dialog_tool.remove(id);
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

    fn models(&mut self, cx: &mut Cx) -> Result<Reply, String> {
        Ok(Reply::Later(self.command_id(
            cx,
            "get_available_models",
            json!({}),
        )))
    }

    fn set_model(&mut self, cx: &mut Cx, model: &str, default: bool) -> Result<Reply, String> {
        if default && self.omp {
            return Err("oh-my-pi keeps no default model through RPC".into());
        }
        let Some((provider, id)) = model.split_once('/') else {
            return Err(format!("{model}: expected provider/model"));
        };
        Ok(Reply::Later(self.command_id(
            cx,
            "set_model",
            json!({"provider": provider, "modelId": id}),
        )))
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
        let approval_dialog = self.dialog_tool.contains_key(id);
        match method {
            "select" if approval_dialog => match Self::approval_option(&req, decision(a)) {
                Some(o) => r["value"] = json!(o),
                None => r["cancelled"] = json!(true),
            },
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
        if self.ready && !self.inflight.values().any(|c| c == "get_session_stats") {
            self.command(cx, "get_session_stats", json!({}));
        }
    }

    fn ready(&self) -> bool {
        self.ready
    }

    fn busy(&self) -> bool {
        self.busy
    }
}
