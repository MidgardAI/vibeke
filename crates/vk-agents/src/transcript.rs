//! Incremental transcript parsing for the TranscriptTailer (04 §10).
//!
//! A [`Parser`] is fed raw bytes in file order (it carries a partial last line over to the next
//! feed) and emits [`Event`]s: turn starts, completed turns with their usage, tool calls and
//! tool results. It is pure: the server owns the file offsets, the `notify` watcher and what
//! happens with the events (dedupe against the structured transport, per-turn records, FTS).
//!
//! Formats: Claude JSONL (`type: user|assistant`, `message.usage`, `tool_use`/`tool_result`
//! blocks), Codex rollout (`event_msg` `user_message`/`token_count`/`task_complete`,
//! `response_item` `function_call`/`function_call_output`) and pi/omp session JSONL (`message`
//! entries with `usage {input, output, cacheRead, cacheWrite, cost{total}}`). The field names are
//! the ones the usage extraction already used; none were re-verified against new versions
//! ([verify M0]).

use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    ClaudeJsonl,
    CodexRollout,
    PiJsonl,
    OmpJsonl,
}

impl Format {
    /// `[transcript] format` of a manifest (`none` and `external:*` have no built-in parser).
    pub fn parse(s: &str) -> Option<Format> {
        match s {
            "claude_jsonl" => Some(Format::ClaudeJsonl),
            "codex_rollout" => Some(Format::CodexRollout),
            "pi_jsonl" => Some(Format::PiJsonl),
            "omp_jsonl" => Some(Format::OmpJsonl),
            _ => None,
        }
    }

    /// Guess from the first lines of a file (when no manifest names the format).
    pub fn sniff(text: &str) -> Option<Format> {
        for l in text.lines().take(20) {
            if l.contains("\"event_msg\"") || l.contains("\"response_item\"") {
                return Some(Format::CodexRollout);
            }
            if l.contains("\"stopReason\"") || l.contains("\"cacheRead\"") {
                return Some(Format::PiJsonl);
            }
            if l.contains("\"type\":\"assistant\"") || l.contains("\"type\": \"assistant\"") {
                return Some(Format::ClaudeJsonl);
            }
        }
        None
    }
}

/// Tokens (and a harness-reported cost) of one turn or a whole file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The harness's own cost figure (pi `cost.total`), summed; `None` when it reports none.
    pub cost_usd: Option<f64>,
}

impl TurnUsage {
    pub fn add(&mut self, o: &TurnUsage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.cost_usd = match (self.cost_usd, o.cost_usd) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
    }

    pub fn is_empty(&self) -> bool {
        self.input == 0 && self.output == 0 && self.cache_read == 0 && self.cache_write == 0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    TurnStarted {
        id: String,
        ts_ms: Option<i64>,
    },
    /// A finished turn. `id` is a native id (Claude message id, Codex turn id, pi entry id) or a
    /// positional fallback; the same id is never emitted twice by one parser.
    Turn {
        id: String,
        n: u32,
        usage: TurnUsage,
        model: Option<String>,
        stop: Option<String>,
        ts_ms: Option<i64>,
    },
    ToolUse {
        id: String,
        name: String,
        command: Option<String>,
        ts_ms: Option<i64>,
    },
    ToolResult {
        id: String,
        is_error: bool,
        ts_ms: Option<i64>,
    },
    /// Background work started: a shell, a monitor or a subagent that outlives its tool call.
    BgStarted {
        id: String,
        kind: String,
        label: String,
        tool_use_id: Option<String>,
        ts_ms: Option<i64>,
    },
    /// Background work ended (`status` is the harness word: completed, failed, stopped, killed).
    BgEnded {
        id: String,
        status: String,
        ts_ms: Option<i64>,
    },
}

/// A Claude `<task-notification>` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskNotification {
    pub task_id: String,
    pub tool_use_id: Option<String>,
    /// `None` for a Monitor event (its stream goes on); a word when the task ended.
    pub status: Option<String>,
    pub summary: String,
}

fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let a = s.find(&open)? + open.len();
    let b = a + s[a..].find(&close)?;
    Some(s[a..b].trim())
}

/// Parse text that is a `<task-notification>` block; `None` when it is anything else.
pub fn parse_task_notification(text: &str) -> Option<TaskNotification> {
    let t = text.trim();
    if !t.starts_with("<task-notification>") {
        return None;
    }
    let task_id = tag(t, "task-id")?.to_string();
    Some(TaskNotification {
        task_id,
        tool_use_id: tag(t, "tool-use-id")
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        status: tag(t, "status")
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        summary: tag(t, "summary").unwrap_or("").to_string(),
    })
}

/// Cap on remembered tool inputs (for background labels).
const INPUTS_MAX: usize = 4000;

/// What the transcript says the harness is doing right now (for `reconcile`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// A prompt or a tool call is pending a response.
    Working,
    /// The last turn ended.
    Idle,
}

#[derive(Debug)]
pub struct Parser {
    format: Format,
    carry: Vec<u8>,
    seen_tools: HashSet<String>,
    seen_bg_start: HashSet<String>,
    seen_bg_end: HashSet<String>,
    /// Tool call id to what it ran (a command or description), for background labels.
    inputs: HashMap<String, String>,
    seen_turns: HashSet<String>,
    seen_msgs: HashSet<String>,
    /// Usage per message id within the open turn (Claude writes one line per content block with
    /// the same id and usage: the last line per id wins).
    msgs: Vec<(String, TurnUsage)>,
    turn_usage: TurnUsage,
    in_turn: bool,
    model: Option<String>,
    turns_done: u32,
    last_codex_total: Option<u64>,
    activity: Option<Activity>,
    totals: TurnUsage,
    /// A title the user gave the session (Claude `/rename`, pi/omp `/name`); wins over `auto_title`.
    user_title: Option<String>,
    /// A title the harness generated (Claude `ai-title`, omp `title`).
    auto_title: Option<String>,
    /// The first prompt, the fallback title.
    first_prompt: Option<String>,
    /// Lines that were not valid JSON.
    pub skipped: u64,
}

impl Parser {
    pub fn new(format: Format) -> Parser {
        Parser {
            format,
            carry: vec![],
            seen_tools: HashSet::new(),
            seen_bg_start: HashSet::new(),
            seen_bg_end: HashSet::new(),
            inputs: HashMap::new(),
            seen_turns: HashSet::new(),
            seen_msgs: HashSet::new(),
            msgs: vec![],
            turn_usage: TurnUsage::default(),
            in_turn: false,
            model: None,
            turns_done: 0,
            last_codex_total: None,
            activity: None,
            totals: TurnUsage::default(),
            user_title: None,
            auto_title: None,
            first_prompt: None,
            skipped: 0,
        }
    }

    pub fn format(&self) -> Format {
        self.format
    }

    pub fn activity(&self) -> Option<Activity> {
        self.activity
    }

    /// Usage of every completed turn so far.
    pub fn totals(&self) -> &TurnUsage {
        &self.totals
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn turns_done(&self) -> u32 {
        self.turns_done
    }

    /// The session's title: one the user set, else one the harness generated, else the first
    /// prompt (one line, at most [`TITLE_MAX`] characters).
    pub fn title(&self) -> Option<&str> {
        self.user_title
            .as_deref()
            .or(self.auto_title.as_deref())
            .or(self.first_prompt.as_deref())
    }

    /// Replace the generated title (omp rewrites its title line in place, so the tailer reads it
    /// from the file head instead of the stream).
    pub fn set_auto_title(&mut self, t: Option<String>) {
        self.auto_title = t;
    }

    fn prompt_seen(&mut self, text: &str) {
        if self.first_prompt.is_none() {
            self.first_prompt = clean_title(text);
        }
    }

    /// Feed the next bytes of the file. A trailing partial line is kept for the next call.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.carry.extend_from_slice(bytes);
        let mut out = vec![];
        let Some(last_nl) = self.carry.iter().rposition(|b| *b == b'\n') else {
            return out;
        };
        let rest = self.carry.split_off(last_nl + 1);
        let complete = std::mem::replace(&mut self.carry, rest);
        for line in complete.split(|b| *b == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<Value>(line) {
                Ok(v) => self.line(&v, &mut out),
                Err(_) => self.skipped += 1,
            }
        }
        out
    }

    /// Bytes of a partial last line waiting for more input.
    pub fn pending_bytes(&self) -> usize {
        self.carry.len()
    }

    fn line(&mut self, v: &Value, out: &mut Vec<Event>) {
        match self.format {
            Format::ClaudeJsonl => self.claude(v, out),
            Format::CodexRollout => self.codex(v, out),
            Format::PiJsonl | Format::OmpJsonl => self.pi(v, out),
        }
    }

    fn start_turn(&mut self, id: String, ts: Option<i64>, out: &mut Vec<Event>) {
        if !self.in_turn {
            self.in_turn = true;
            self.msgs.clear();
            self.turn_usage = TurnUsage::default();
        }
        self.activity = Some(Activity::Working);
        out.push(Event::TurnStarted { id, ts_ms: ts });
    }

    fn finish_turn(
        &mut self,
        id: String,
        stop: Option<String>,
        ts: Option<i64>,
        out: &mut Vec<Event>,
    ) {
        let mut usage = self.turn_usage.clone();
        for (_, u) in &self.msgs {
            usage.add(u);
        }
        self.msgs.clear();
        self.turn_usage = TurnUsage::default();
        self.in_turn = false;
        self.activity = Some(Activity::Idle);
        if !self.seen_turns.insert(id.clone()) {
            return;
        }
        self.turns_done += 1;
        self.totals.add(&usage);
        out.push(Event::Turn {
            id,
            n: self.turns_done,
            usage,
            model: self.model.clone(),
            stop,
            ts_ms: ts,
        });
    }

    fn tool_use(
        &mut self,
        id: &str,
        name: &str,
        command: Option<String>,
        ts: Option<i64>,
        out: &mut Vec<Event>,
    ) {
        if id.is_empty() || !self.seen_tools.insert(id.to_string()) {
            return;
        }
        out.push(Event::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            command,
            ts_ms: ts,
        });
    }

    fn remember_input(&mut self, id: &str, text: Option<&str>) {
        let Some(t) = text.filter(|t| !id.is_empty() && !t.trim().is_empty()) else {
            return;
        };
        if self.inputs.len() >= INPUTS_MAX {
            self.inputs.clear();
        }
        self.inputs.insert(id.to_string(), t.to_string());
    }

    fn bg_started(
        &mut self,
        id: &str,
        kind: &str,
        tool_use_id: Option<&str>,
        label: Option<String>,
        ts: Option<i64>,
        out: &mut Vec<Event>,
    ) {
        if id.is_empty() || !self.seen_bg_start.insert(id.to_string()) {
            return;
        }
        let label = label
            .or_else(|| tool_use_id.and_then(|t| self.inputs.get(t).cloned()))
            .unwrap_or_default();
        out.push(Event::BgStarted {
            id: id.to_string(),
            kind: kind.to_string(),
            label,
            tool_use_id: tool_use_id.map(str::to_string),
            ts_ms: ts,
        });
    }

    fn bg_ended(&mut self, id: &str, status: &str, ts: Option<i64>, out: &mut Vec<Event>) {
        if id.is_empty() || !self.seen_bg_end.insert(id.to_string()) {
            return;
        }
        out.push(Event::BgEnded {
            id: id.to_string(),
            status: status.to_string(),
            ts_ms: ts,
        });
    }

    /// Background records of a Claude `user` line.
    fn claude_bg(&mut self, v: &Value, ts: Option<i64>, out: &mut Vec<Event>) {
        let content = v.pointer("/message/content");
        let notification = v.pointer("/origin/kind").and_then(Value::as_str)
            == Some("task-notification")
            || user_text(content)
                .is_some_and(|t| t.trim_start().starts_with("<task-notification>"));
        if notification {
            if let Some(n) = user_text(content).and_then(|t| parse_task_notification(&t))
                && let Some(status) = n.status
            {
                self.bg_ended(&n.task_id, &status, ts, out);
            }
            return;
        }
        let Some(r) = v.get("toolUseResult").filter(|r| r.is_object()) else {
            return;
        };
        let tool_use_id = match content {
            Some(Value::Array(blocks)) => blocks
                .iter()
                .find(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                .and_then(|b| b.get("tool_use_id"))
                .and_then(Value::as_str),
            _ => None,
        };
        let s = |k: &str| r.get(k).and_then(Value::as_str);
        if let Some(id) = s("backgroundTaskId") {
            self.bg_started(id, "shell", tool_use_id, None, ts, out);
        } else if let Some(id) = s("taskId")
            && r.get("timeoutMs").is_some()
        {
            self.bg_started(id, "monitor", tool_use_id, None, ts, out);
        } else if s("status") == Some("async_launched")
            && let Some(id) = s("agentId")
        {
            let label = s("description").map(str::to_string);
            self.bg_started(id, "subagent", tool_use_id, label, ts, out);
        } else if let Some(id) = s("task_id")
            && s("message").is_some_and(|m| m.starts_with("Successfully stopped"))
        {
            self.bg_ended(id, "stopped", ts, out);
        }
    }

    // ---- Claude ----------------------------------------------------------------------------

    fn claude(&mut self, v: &Value, out: &mut Vec<Event>) {
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms);
        let sidechain = v.get("isSidechain").and_then(Value::as_bool) == Some(true);
        match v.get("type").and_then(Value::as_str) {
            Some("user") => {
                let content = v.pointer("/message/content");
                if !sidechain {
                    self.claude_bg(v, ts, out);
                }
                let mut had_result = false;
                let mut had_text = false;
                match content {
                    Some(Value::String(s)) => had_text = !s.trim().is_empty(),
                    Some(Value::Array(blocks)) => {
                        for b in blocks {
                            match b.get("type").and_then(Value::as_str) {
                                Some("tool_result") => {
                                    had_result = true;
                                    out.push(Event::ToolResult {
                                        id: b
                                            .get("tool_use_id")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                        is_error: b.get("is_error").and_then(Value::as_bool)
                                            == Some(true),
                                        ts_ms: ts,
                                    });
                                }
                                Some("text") => had_text = true,
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                let meta = v.get("isMeta").and_then(Value::as_bool) == Some(true);
                if had_text && !had_result && !sidechain && !meta {
                    if let Some(t) = user_text(content) {
                        self.prompt_seen(&t);
                    }
                    let id = v
                        .get("uuid")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("prompt{}", self.turns_done + 1));
                    self.start_turn(id, ts, out);
                } else if had_result && !sidechain {
                    self.activity = Some(Activity::Working);
                }
            }
            Some("assistant") => {
                let msg_id = v
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(m) = v.pointer("/message/model").and_then(Value::as_str)
                    && m != "<synthetic>"
                {
                    self.model = Some(m.to_string());
                }
                if let Some(u) = v.pointer("/message/usage") {
                    let tu = TurnUsage {
                        input: n64(u, "input_tokens"),
                        output: n64(u, "output_tokens"),
                        cache_read: n64(u, "cache_read_input_tokens"),
                        cache_write: n64(u, "cache_creation_input_tokens"),
                        cost_usd: v.get("costUSD").and_then(Value::as_f64),
                    };
                    let id = msg_id
                        .clone()
                        .unwrap_or_else(|| format!("anon{}", self.msgs.len()));
                    self.in_turn = true;
                    match self.msgs.iter_mut().find(|(i, _)| *i == id) {
                        Some(e) => e.1 = tu,
                        None => self.msgs.push((id, tu)),
                    }
                }
                if let Some(Value::Array(blocks)) = v.pointer("/message/content") {
                    for b in blocks {
                        if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                            let id = b.get("id").and_then(Value::as_str).unwrap_or("");
                            let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let cmd = b
                                .pointer("/input/command")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            let remembered = cmd.clone().or_else(|| {
                                b.pointer("/input/description")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            });
                            self.remember_input(id, remembered.as_deref());
                            self.tool_use(id, name, cmd, ts, out);
                        }
                    }
                }
                let stop = v
                    .pointer("/message/stop_reason")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match stop.as_deref() {
                    Some("tool_use") => self.activity = Some(Activity::Working),
                    Some(_) if !sidechain => {
                        let id = msg_id.unwrap_or_else(|| format!("turn{}", self.turns_done + 1));
                        self.finish_turn(id, stop, ts, out);
                    }
                    _ => {
                        if !sidechain {
                            self.activity = Some(Activity::Working);
                        }
                    }
                }
            }
            Some("custom-title") => {
                self.user_title = v
                    .get("customTitle")
                    .and_then(Value::as_str)
                    .and_then(clean_title);
            }
            Some("ai-title") => {
                if let Some(t) = v
                    .get("aiTitle")
                    .and_then(Value::as_str)
                    .and_then(clean_title)
                {
                    self.auto_title = Some(t);
                }
            }
            _ => {}
        }
    }

    // ---- Codex ---------------------------------------------------------------------------

    fn codex(&mut self, v: &Value, out: &mut Vec<Event>) {
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms);
        let p = &v["payload"];
        let pty = p.get("type").and_then(Value::as_str).unwrap_or("");
        match v.get("type").and_then(Value::as_str) {
            Some("turn_context") => {
                if let Some(m) = p.get("model").and_then(Value::as_str) {
                    self.model = Some(m.to_string());
                }
            }
            Some("event_msg") => match pty {
                "user_message" | "task_started" | "turn_started" => {
                    if pty == "user_message"
                        && let Some(m) = p.get("message").and_then(Value::as_str)
                    {
                        self.prompt_seen(m);
                    }
                    if pty == "user_message"
                        && self.in_turn
                        && self.activity == Some(Activity::Working)
                    {
                        // A second prompt inside an open turn joins it.
                        return;
                    }
                    let id = p
                        .get("turn_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("prompt{}", self.turns_done + 1));
                    self.start_turn(id, ts, out);
                }
                "token_count" => {
                    let Some(t) = p.pointer("/info/last_token_usage") else {
                        return;
                    };
                    let total = p
                        .pointer("/info/total_token_usage/total_tokens")
                        .and_then(Value::as_u64);
                    // Codex writes the same token_count twice in a row.
                    if total.is_some() && total == self.last_codex_total {
                        return;
                    }
                    self.last_codex_total = total;
                    let cached = n64(t, "cached_input_tokens");
                    self.in_turn = true;
                    self.turn_usage.add(&TurnUsage {
                        input: n64(t, "input_tokens").saturating_sub(cached),
                        output: n64(t, "output_tokens") + n64(t, "reasoning_output_tokens"),
                        cache_read: cached,
                        cache_write: 0,
                        cost_usd: None,
                    });
                }
                "task_complete" | "turn_complete" | "turn_aborted" => {
                    let id = p
                        .get("turn_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("turn{}", self.turns_done + 1));
                    self.finish_turn(id, Some(pty.to_string()), ts, out);
                }
                "item_completed" => {
                    let item = &p["item"];
                    if item.get("type").and_then(Value::as_str) == Some("CommandExecution")
                        && let Some(pid) = item.get("process_id").and_then(|x| {
                            x.as_str()
                                .map(str::to_string)
                                .or_else(|| x.as_u64().map(|n| n.to_string()))
                        })
                    {
                        let status = item
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("completed");
                        self.bg_ended(&pid, status, ts, out);
                    }
                }
                "exec_command_end" => {
                    let id = p.get("call_id").and_then(Value::as_str).unwrap_or("");
                    let code = p.get("exit_code").and_then(Value::as_i64).unwrap_or(0);
                    out.push(Event::ToolResult {
                        id: id.to_string(),
                        is_error: code != 0,
                        ts_ms: ts,
                    });
                }
                _ => {}
            },
            Some("response_item") => match pty {
                "function_call" | "local_shell_call" | "custom_tool_call" => {
                    let id = p
                        .get("call_id")
                        .or_else(|| p.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let name = p.get("name").and_then(Value::as_str).unwrap_or(
                        if pty == "local_shell_call" {
                            "shell"
                        } else {
                            "tool"
                        },
                    );
                    let args: Value = p
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|a| serde_json::from_str(a).ok())
                        .unwrap_or(Value::Null);
                    let cmd = join_command(
                        args.get("command")
                            .or_else(|| args.get("cmd"))
                            .or_else(|| p.pointer("/action/command")),
                    );
                    self.activity = Some(Activity::Working);
                    self.remember_input(id, cmd.as_deref());
                    self.tool_use(id, name, cmd, ts, out);
                }
                "function_call_output" | "custom_tool_call_output" => {
                    let id = p.get("call_id").and_then(Value::as_str).unwrap_or("");
                    let is_error = p
                        .get("output")
                        .and_then(Value::as_str)
                        .and_then(|o| serde_json::from_str::<Value>(o).ok())
                        .and_then(|o| o.pointer("/metadata/exit_code").and_then(Value::as_i64))
                        .is_some_and(|c| c != 0);
                    out.push(Event::ToolResult {
                        id: id.to_string(),
                        is_error,
                        ts_ms: ts,
                    });
                    for sid in codex_session_ids(pty, p.get("output")) {
                        self.bg_started(&sid, "shell", Some(id), None, ts, out);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    // ---- pi / omp --------------------------------------------------------------------------

    fn pi(&mut self, v: &Value, out: &mut Vec<Event>) {
        match v.get("type").and_then(Value::as_str) {
            Some("message") => {}
            // `/name`: the latest entry wins, an empty name clears it.
            Some("session_info") => {
                self.user_title = v.get("name").and_then(Value::as_str).and_then(clean_title);
                return;
            }
            // omp's title header (also read from the file head, see `head_title`).
            Some("title") => {
                self.auto_title = v.get("title").and_then(Value::as_str).and_then(clean_title);
                return;
            }
            _ => return,
        }
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms);
        let m = &v["message"];
        let entry_id = v.get("id").and_then(Value::as_str).map(str::to_string);
        match m.get("role").and_then(Value::as_str) {
            Some("user") => {
                if let Some(t) = user_text(m.get("content")) {
                    self.prompt_seen(&t);
                }
                let id = entry_id.unwrap_or_else(|| format!("prompt{}", self.turns_done + 1));
                self.start_turn(id, ts, out);
            }
            Some("toolResult") => {
                out.push(Event::ToolResult {
                    id: m
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    is_error: m.get("isError").and_then(Value::as_bool) == Some(true),
                    ts_ms: ts,
                });
                self.activity = Some(Activity::Working);
            }
            Some("assistant") => {
                if let Some(model) = m.get("model").and_then(Value::as_str) {
                    self.model = Some(model.to_string());
                }
                let id = entry_id.unwrap_or_else(|| format!("msg{}", self.seen_msgs.len()));
                if let Some(u) = m.get("usage")
                    && self.seen_msgs.insert(id.clone())
                {
                    self.in_turn = true;
                    self.turn_usage.add(&TurnUsage {
                        input: n64(u, "input"),
                        output: n64(u, "output"),
                        cache_read: n64(u, "cacheRead"),
                        cache_write: n64(u, "cacheWrite"),
                        cost_usd: u.pointer("/cost/total").and_then(Value::as_f64),
                    });
                }
                if let Some(Value::Array(blocks)) = m.get("content") {
                    for b in blocks {
                        if matches!(
                            b.get("type").and_then(Value::as_str),
                            Some("toolCall" | "tool_use")
                        ) {
                            let tid = b.get("id").and_then(Value::as_str).unwrap_or("");
                            let name = b.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let args = b.get("arguments").or_else(|| b.get("input"));
                            let cmd = args
                                .and_then(|a| a.get("command"))
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            self.tool_use(tid, name, cmd, ts, out);
                        }
                    }
                }
                match m.get("stopReason").and_then(Value::as_str) {
                    Some("toolUse") => self.activity = Some(Activity::Working),
                    Some(s) => self.finish_turn(id, Some(s.to_string()), ts, out),
                    None => {}
                }
            }
            _ => {}
        }
    }
}

/// Longest title kept, in characters.
pub const TITLE_MAX: usize = 80;

/// One line, redacted, trimmed, at most [`TITLE_MAX`] characters (cut at a word when possible). Prompts that
/// start with a harness wrapper (`<command-name>`, `<task-notification>`…) are not titles.
fn clean_title(s: &str) -> Option<String> {
    // Redact before cutting, so a cut cannot leave half a secret unmatched.
    let line = vk_redact::redact(s)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if line.is_empty() || line.starts_with('<') {
        return None;
    }
    if line.chars().count() <= TITLE_MAX {
        return Some(line);
    }
    let cut: String = line.chars().take(TITLE_MAX - 1).collect();
    let cut = match cut.rfind(' ') {
        Some(i) if i > TITLE_MAX / 2 => &cut[..i],
        _ => &cut[..],
    };
    Some(format!("{}…", cut.trim_end()))
}

/// The text of a user message: a string, or its `text` blocks joined.
fn user_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let t: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            (!t.is_empty()).then(|| t.join(" "))
        }
        _ => None,
    }
}

/// omp's title from the head of its session file: the first line is a padded `title` entry the
/// harness rewrites in place, so an incremental tail never sees the update. `None` when the first
/// line cannot be read as JSON (mid-rewrite): keep the title you had. `Some(None)`: no title.
pub fn head_title(head: &[u8]) -> Option<Option<String>> {
    let line = head.split(|b| *b == b'\n').next()?;
    let v: Value = serde_json::from_slice(line).ok()?;
    if v.get("type").and_then(Value::as_str) != Some("title") {
        return Some(None);
    }
    Some(v.get("title").and_then(Value::as_str).and_then(clean_title))
}

/// Codex's thread name for `session` from `session_index.jsonl` (one `{id, thread_name}` line
/// per rename; the last one for the id wins).
pub fn codex_thread_name(index: &str, session: &str) -> Option<String> {
    let mut name = None;
    for l in index.lines() {
        if !l.contains(session) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        if v.get("id").and_then(Value::as_str) == Some(session) {
            name = v
                .get("thread_name")
                .and_then(Value::as_str)
                .and_then(clean_title);
        }
    }
    name
}

/// Session ids of Codex commands still running after their call returned: the legacy
/// `exec_command` text says `Process running with session ID N`; code-mode `exec` answers JSON
/// objects (one per command it ran) with `"session_id": N` and no `exit_code`.
fn codex_session_ids(pty: &str, output: Option<&Value>) -> Vec<String> {
    let text = match output {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return vec![],
    };
    if pty == "function_call_output" {
        return text
            .split("Process running with session ID ")
            .skip(1)
            .map(|rest| {
                rest.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .filter(|n| !n.is_empty())
            .collect();
    }
    let mut ids = Vec::new();
    let mut objects = vec![];
    // Each text block may lead with prose; its JSON starts at the first brace or bracket.
    for block in text.split('\n') {
        let Some(at) = block.find(['{', '[']) else {
            continue;
        };
        for v in serde_json::Deserializer::from_str(&block[at..]).into_iter::<Value>() {
            match v {
                Ok(Value::Array(a)) => objects.extend(a),
                Ok(v) => objects.push(v),
                Err(_) => break,
            }
        }
    }
    for o in objects {
        if o.get("exit_code").is_some_and(|c| !c.is_null()) {
            continue;
        }
        let id = match o.get("session_id") {
            Some(Value::Number(n)) => n.to_string(),
            Some(Value::String(s)) => s.clone(),
            _ => continue,
        };
        ids.push(id);
    }
    ids
}

fn n64(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn join_command(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => Some(
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

/// `2026-10-06T12:00:00.123Z` → epoch milliseconds (UTC only; enough for transcript lines).
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, mo, da) = (d.next()??, d.next()??, d.next()??);
    let time = time.trim_end_matches('Z');
    let time = time.split(['+']).next().unwrap_or(time);
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':').map(|x| x.parse::<i64>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next()??);
    let ms: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
    // Days since 1970-01-01 (civil-from-days inverse).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + da - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + se * 1000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lines(v: &[Value]) -> Vec<u8> {
        let mut b = vec![];
        for x in v {
            b.extend(serde_json::to_vec(x).unwrap());
            b.push(b'\n');
        }
        b
    }

    fn turns(ev: &[Event]) -> Vec<(&str, &TurnUsage)> {
        ev.iter()
            .filter_map(|e| match e {
                Event::Turn { id, usage, .. } => Some((id.as_str(), usage)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn titles_prefer_the_users_then_the_harness_then_the_first_prompt() {
        let mut p = Parser::new(Format::ClaudeJsonl);
        p.feed(&lines(&[
            json!({"type":"user","uuid":"u0","message":{"role":"user","content":"<command-name>/clear</command-name>"}}),
            json!({"type":"user","uuid":"u1","message":{"role":"user","content":[{"type":"text","text":"  fix the\n reconnect bug  "}]}}),
        ]));
        assert_eq!(p.title(), Some("fix the reconnect bug"));
        p.feed(&lines(&[
            json!({"type":"ai-title","aiTitle":"PWA reconnect error"}),
        ]));
        assert_eq!(p.title(), Some("PWA reconnect error"));
        p.feed(&lines(&[
            json!({"type":"custom-title","customTitle":"reconnect"}),
            json!({"type":"ai-title","aiTitle":"Later generated"}),
        ]));
        assert_eq!(p.title(), Some("reconnect"));

        let mut pi = Parser::new(Format::PiJsonl);
        pi.feed(&lines(&[
            json!({"type":"message","id":"e1","message":{"role":"user","content":[{"type":"text","text":"add tests"}]}}),
            json!({"type":"session_info","name":"Test pass"}),
        ]));
        assert_eq!(pi.title(), Some("Test pass"));
        pi.feed(&lines(&[json!({"type":"session_info","name":""})]));
        assert_eq!(pi.title(), Some("add tests"));

        let mut codex = Parser::new(Format::CodexRollout);
        codex.feed(&lines(&[
            json!({"type":"event_msg","payload":{"type":"user_message","message":"plan the update flow"}}),
        ]));
        assert_eq!(codex.title(), Some("plan the update flow"));
    }

    #[test]
    fn long_titles_are_cut_at_a_word() {
        let t = clean_title(&"word ".repeat(40)).unwrap();
        assert!(t.chars().count() <= TITLE_MAX);
        assert!(t.ends_with("word…"));
        assert_eq!(clean_title("   "), None);
        let t = clean_title("deploy with password=hunter2secret please").unwrap();
        assert!(!t.contains("hunter2secret"), "{t}");
    }

    #[test]
    fn omp_head_and_codex_index_titles() {
        let head = b"{\"type\":\"title\",\"v\":1,\"title\":\"Describe the brand\",\"source\":\"auto\",\"pad\":\"   \"}\n{\"type\":\"session\"}\n";
        assert_eq!(head_title(head), Some(Some("Describe the brand".into())));
        assert_eq!(head_title(b"{\"type\":\"session\"}\n"), Some(None));
        assert_eq!(head_title(b"{\"type\":\"tit"), None);
        let index = "{\"id\":\"a\",\"thread_name\":\"First\"}\n{\"id\":\"b\",\"thread_name\":\"Other\"}\n{\"id\":\"a\",\"thread_name\":\"Renamed\"}\n";
        assert_eq!(codex_thread_name(index, "a").as_deref(), Some("Renamed"));
        assert_eq!(codex_thread_name(index, "c"), None);
    }

    #[test]
    fn rfc3339_parses_to_epoch_ms() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2026-10-06T12:00:00.123Z"),
            Some(1_791_288_000_123)
        );
        assert_eq!(
            parse_rfc3339_ms("2000-03-01T00:00:01.5Z"),
            Some(951_868_801_500)
        );
        assert!(parse_rfc3339_ms("nonsense").is_none());
    }

    fn claude_turn() -> Vec<Value> {
        vec![
            serde_json::json!({"type":"user","uuid":"u1","timestamp":"2026-10-06T12:00:00Z","message":{"role":"user","content":"fix it"}}),
            serde_json::json!({"type":"assistant","timestamp":"2026-10-06T12:00:01Z","message":{"id":"m1","model":"claude-sonnet-4","stop_reason":null,"usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":100,"cache_creation_input_tokens":5},"content":[{"type":"text","text":"ok"}]}}),
            serde_json::json!({"type":"assistant","timestamp":"2026-10-06T12:00:02Z","message":{"id":"m1","model":"claude-sonnet-4","stop_reason":"tool_use","usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":100,"cache_creation_input_tokens":5},"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}),
            serde_json::json!({"type":"user","timestamp":"2026-10-06T12:00:03Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}),
            serde_json::json!({"type":"assistant","timestamp":"2026-10-06T12:00:04Z","message":{"id":"m2","model":"claude-sonnet-4","stop_reason":"end_turn","usage":{"input_tokens":30,"output_tokens":40,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"done"}]}}),
        ]
    }

    #[test]
    fn claude_turns_dedupe_message_ids_and_sum_usage() {
        let mut p = Parser::new(Format::ClaudeJsonl);
        let ev = p.feed(&lines(&claude_turn()));
        let t = turns(&ev);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, "m2");
        // m1 counted once (the later line wins) + m2.
        assert_eq!(t[0].1.input, 40);
        assert_eq!(t[0].1.output, 60);
        assert_eq!(t[0].1.cache_read, 100);
        assert_eq!(t[0].1.cache_write, 5);
        assert!(
            ev.iter()
                .any(|e| matches!(e, Event::TurnStarted { id, .. } if id == "u1"))
        );
        assert!(ev.iter().any(|e| matches!(e, Event::ToolUse { id, command: Some(c), .. } if id == "t1" && c == "cargo test")));
        assert!(
            ev.iter()
                .any(|e| matches!(e, Event::ToolResult { id, is_error: false, .. } if id == "t1"))
        );
        assert_eq!(p.activity(), Some(Activity::Idle));
        assert_eq!(p.model(), Some("claude-sonnet-4"));
        assert_eq!(p.totals().output, 60);
        // Feeding the same lines again (a re-read) emits no second turn or tool call.
        let again = p.feed(&lines(&claude_turn()));
        assert!(turns(&again).is_empty());
        assert!(!again.iter().any(|e| matches!(e, Event::ToolUse { .. })));
    }

    #[test]
    fn partial_lines_are_carried_over() {
        let all = lines(&claude_turn());
        let mid = all.len() / 2;
        let mut p = Parser::new(Format::ClaudeJsonl);
        let mut ev = p.feed(&all[..mid]);
        assert!(p.pending_bytes() > 0);
        ev.extend(p.feed(&all[mid..]));
        assert_eq!(turns(&ev).len(), 1);
        assert_eq!(p.pending_bytes(), 0);
        // Junk lines are skipped, not fatal.
        let ev = p.feed(b"not json\n\n");
        assert!(ev.is_empty());
        assert_eq!(p.skipped, 1);
    }

    #[test]
    fn claude_activity_follows_the_last_line() {
        let mut p = Parser::new(Format::ClaudeJsonl);
        let mut v = claude_turn();
        v.truncate(3);
        p.feed(&lines(&v));
        assert_eq!(p.activity(), Some(Activity::Working), "tool call pending");
        let mut p = Parser::new(Format::ClaudeJsonl);
        p.feed(&lines(&claude_turn()[..1]));
        assert_eq!(p.activity(), Some(Activity::Working), "prompt submitted");
    }

    #[test]
    fn claude_sidechains_add_usage_but_never_end_the_turn() {
        let mut p = Parser::new(Format::ClaudeJsonl);
        let mut v = vec![claude_turn()[0].clone()];
        v.push(serde_json::json!({"type":"assistant","isSidechain":true,"message":{"id":"s1","stop_reason":"end_turn","usage":{"input_tokens":7,"output_tokens":7},"content":[]}}));
        let ev = p.feed(&lines(&v));
        assert!(turns(&ev).is_empty());
        v = claude_turn()[4..].to_vec();
        let ev = p.feed(&lines(&v));
        assert_eq!(turns(&ev)[0].1.input, 37, "sidechain tokens are spend too");
    }

    #[test]
    fn codex_rollout_turn_usage_and_tools() {
        let mut p = Parser::new(Format::CodexRollout);
        let v = vec![
            serde_json::json!({"timestamp":"2026-10-06T12:00:00Z","type":"turn_context","payload":{"model":"gpt-5-codex"}}),
            serde_json::json!({"timestamp":"2026-10-06T12:00:00Z","type":"event_msg","payload":{"type":"user_message","message":"hi"}}),
            serde_json::json!({"timestamp":"2026-10-06T12:00:01Z","type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"ls\"]}"}}),
            serde_json::json!({"timestamp":"2026-10-06T12:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"{\"output\":\"x\",\"metadata\":{\"exit_code\":2}}"}}),
            serde_json::json!({"timestamp":"2026-10-06T12:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":50,"reasoning_output_tokens":10},"total_token_usage":{"total_tokens":1060}}}}),
            // Duplicate of the same token_count: ignored.
            serde_json::json!({"timestamp":"2026-10-06T12:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":1000,"cached_input_tokens":400,"output_tokens":50,"reasoning_output_tokens":10},"total_token_usage":{"total_tokens":1060}}}}),
            serde_json::json!({"timestamp":"2026-10-06T12:00:04Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":200,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0},"total_token_usage":{"total_tokens":1265}}}}),
            serde_json::json!({"timestamp":"2026-10-06T12:00:05Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"tk1"}}),
        ];
        let ev = p.feed(&lines(&v));
        let t = turns(&ev);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].0, "tk1");
        assert_eq!(t[0].1.input, 600 + 200);
        assert_eq!(t[0].1.cache_read, 400);
        assert_eq!(t[0].1.output, 60 + 5);
        assert!(ev.iter().any(|e| matches!(e, Event::ToolUse { id, command: Some(c), .. } if id == "c1" && c == "bash -lc ls")));
        assert!(
            ev.iter()
                .any(|e| matches!(e, Event::ToolResult { id, is_error: true, .. } if id == "c1"))
        );
        assert_eq!(p.model(), Some("gpt-5-codex"));
        assert_eq!(p.activity(), Some(Activity::Idle));
    }

    #[test]
    fn pi_entries_sum_usage_and_cost() {
        let mut p = Parser::new(Format::PiJsonl);
        let v = vec![
            serde_json::json!({"type":"message","id":"e1","timestamp":"2026-10-06T12:00:00Z","message":{"role":"user","content":"go"}}),
            serde_json::json!({"type":"message","id":"e2","timestamp":"2026-10-06T12:00:01Z","message":{"role":"assistant","model":"claude-sonnet-4","stopReason":"toolUse","usage":{"input":10,"output":5,"cacheRead":1,"cacheWrite":2,"cost":{"total":0.01}},"content":[{"type":"toolCall","id":"tc1","name":"bash","arguments":{"command":"ls"}}]}}),
            serde_json::json!({"type":"message","id":"e3","message":{"role":"toolResult","toolCallId":"tc1","isError":false}}),
            serde_json::json!({"type":"message","id":"e4","timestamp":"2026-10-06T12:00:03Z","message":{"role":"assistant","stopReason":"stop","usage":{"input":20,"output":15,"cacheRead":0,"cacheWrite":0,"cost":{"total":0.02}},"content":[]}}),
        ];
        let ev = p.feed(&lines(&v));
        let t = turns(&ev);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].1.input, 30);
        assert_eq!(t[0].1.output, 20);
        assert!((t[0].1.cost_usd.unwrap() - 0.03).abs() < 1e-9);
        assert!(
            ev.iter()
                .any(|e| matches!(e, Event::ToolUse { id, .. } if id == "tc1"))
        );
        // omp uses the same entries.
        let mut o = Parser::new(Format::OmpJsonl);
        assert_eq!(turns(&o.feed(&lines(&v))).len(), 1);
    }

    #[test]
    fn format_names_and_sniffing() {
        assert_eq!(Format::parse("claude_jsonl"), Some(Format::ClaudeJsonl));
        assert_eq!(Format::parse("none"), None);
        assert_eq!(Format::parse("external:x"), None);
        let claude = String::from_utf8(lines(&claude_turn())).unwrap();
        assert_eq!(Format::sniff(&claude), Some(Format::ClaudeJsonl));
        assert_eq!(
            Format::sniff("{\"type\":\"event_msg\",\"payload\":{}}"),
            Some(Format::CodexRollout)
        );
        assert_eq!(
            Format::sniff("{\"message\":{\"stopReason\":\"stop\"}}"),
            Some(Format::PiJsonl)
        );
        assert_eq!(Format::sniff("hello"), None);
    }

    fn bg(ev: &[Event]) -> Vec<&Event> {
        ev.iter()
            .filter(|e| matches!(e, Event::BgStarted { .. } | Event::BgEnded { .. }))
            .collect()
    }

    fn notification(id: &str, status: Option<&str>) -> Value {
        let st = status
            .map(|s| format!("<status>{s}</status>\n"))
            .unwrap_or_default();
        json!({"type":"user","timestamp":"2026-01-01T00:00:09Z",
            "origin":{"kind":"task-notification"},
            "message":{"role":"user","content":format!("<task-notification>\n<task-id>{id}</task-id>\n<tool-use-id>tu1</tool-use-id>\n{st}<summary>Background command done</summary>\n</task-notification>")}})
    }

    #[test]
    fn claude_background_shell_starts_and_ends() {
        let mut p = Parser::new(Format::ClaudeJsonl);
        let ev = p.feed(&lines(&[
            json!({"type":"assistant","message":{"id":"m1","stop_reason":"tool_use","content":[
                {"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"sleep 30","description":"wait","run_in_background":true}}]}}),
            json!({"type":"user","timestamp":"2026-01-01T00:00:01Z",
                "toolUseResult":{"stdout":"","backgroundTaskId":"bg1"},
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":"running"}]}}),
            notification("bg1", Some("completed")),
        ]));
        let b = bg(&ev);
        assert_eq!(b.len(), 2, "{b:?}");
        assert!(
            matches!(b[0], Event::BgStarted { id, kind, label, tool_use_id, ts_ms }
            if id == "bg1" && kind == "shell" && label == "sleep 30"
                && tool_use_id.as_deref() == Some("tu1") && ts_ms.is_some())
        );
        assert!(
            matches!(b[1], Event::BgEnded { id, status, .. } if id == "bg1" && status == "completed")
        );
        // The same lines again emit nothing new.
        let again = p.feed(&lines(&[notification("bg1", Some("completed"))]));
        assert!(bg(&again).is_empty());
    }

    #[test]
    fn claude_monitor_and_subagent_start() {
        let mut p = Parser::new(Format::ClaudeJsonl);
        let ev = p.feed(&lines(&[
            json!({"type":"assistant","message":{"id":"m1","stop_reason":"tool_use","content":[
                {"type":"tool_use","id":"tm","name":"Monitor","input":{"description":"watch logs","command":"tail -f x"}}]}}),
            json!({"type":"user","toolUseResult":{"taskId":"mon1","timeoutMs":60000,"persistent":false},
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tm","content":"started"}]}}),
            json!({"type":"user","toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"ag1","description":"Look around"},
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ta","content":"launched"}]}}),
        ]));
        let b = bg(&ev);
        assert_eq!(b.len(), 2, "{b:?}");
        assert!(matches!(b[0], Event::BgStarted { id, kind, label, .. }
            if id == "mon1" && kind == "monitor" && label == "tail -f x"));
        assert!(
            matches!(b[1], Event::BgStarted { id, kind, label, tool_use_id, .. }
            if id == "ag1" && kind == "subagent" && label == "Look around"
                && tool_use_id.as_deref() == Some("ta"))
        );
    }

    #[test]
    fn claude_monitor_event_is_not_an_end() {
        let mut p = Parser::new(Format::ClaudeJsonl);
        let ev = p.feed(&lines(&[notification("mon1", None)]));
        assert!(bg(&ev).is_empty());
        // The notification still starts a turn (the agent wakes up).
        assert!(ev.iter().any(|e| matches!(e, Event::TurnStarted { .. })));
        // Fallback without `origin`: string content starting with the tag.
        let mut q = Parser::new(Format::ClaudeJsonl);
        let mut n = notification("mon2", Some("killed"));
        n.as_object_mut().unwrap().remove("origin");
        let ev = q.feed(&lines(&[n]));
        assert!(
            matches!(bg(&ev)[0], Event::BgEnded { id, status, .. } if id == "mon2" && status == "killed")
        );
    }

    #[test]
    fn claude_task_stop_ends() {
        let mut p = Parser::new(Format::ClaudeJsonl);
        let ev = p.feed(&lines(&[
            json!({"type":"user","toolUseResult":{"message":"Successfully stopped task: bg9 (sleep)","task_id":"bg9","task_type":"local_bash"},
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tx","content":"ok"}]}}),
            json!({"type":"user","toolUseResult":{"message":"No task found","task_id":"bg8"},
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ty","content":"no"}]}}),
        ]));
        let b = bg(&ev);
        assert_eq!(b.len(), 1);
        assert!(
            matches!(b[0], Event::BgEnded { id, status, .. } if id == "bg9" && status == "stopped")
        );
    }

    #[test]
    fn parses_task_notifications() {
        let n = parse_task_notification(
            "<task-notification>\n<task-id>a1</task-id>\n<status>failed</status>\n<summary>It broke</summary>\n</task-notification>",
        )
        .unwrap();
        assert_eq!(n.task_id, "a1");
        assert_eq!(n.status.as_deref(), Some("failed"));
        assert_eq!(n.tool_use_id, None);
        assert_eq!(n.summary, "It broke");
        assert!(parse_task_notification("hello <task-notification>").is_none());
    }

    #[test]
    fn codex_background_sessions_start_and_end() {
        let mut p = Parser::new(Format::CodexRollout);
        let ev = p.feed(&lines(&[
            json!({"timestamp":"2026-01-01T00:00:00Z","type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"exec_command","arguments":"{\"cmd\":\"npm run dev\"}"}}),
            json!({"timestamp":"2026-01-01T00:00:01Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"Wall time: 1 seconds\nProcess running with session ID 4242\nOutput:\n"}}),
            json!({"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c2","output":"Process running with session ID 4242\nOutput:\n"}}),
            json!({"timestamp":"2026-01-01T00:00:03Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c3","output":[{"type":"input_text","text":"{\"session_id\": 77, \"output\": \"\"}"}]}}),
            json!({"timestamp":"2026-01-01T00:00:04Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c4","output":[{"type":"input_text","text":"{\"session_id\": 78, \"exit_code\": 0}"}]}}),
            json!({"timestamp":"2026-01-01T00:00:05Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c5","output":[{"type":"input_text","text":"{\"session_id\": 79, \"exit_code\": 0}"},{"type":"input_text","text":"{\"session_id\": 80, \"output\": \"\"}"},{"type":"input_text","text":"{\"session_id\": 81}"}]}}),
            json!({"timestamp":"2026-01-01T00:00:05Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"e1","process_id":"4242","status":"completed"}}}),
            json!({"timestamp":"2026-01-01T00:00:06Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"Plan","id":"e2"}}}),
        ]));
        let b = bg(&ev);
        assert_eq!(b.len(), 5, "{b:?}");
        assert!(
            matches!(b[0], Event::BgStarted { id, kind, label, tool_use_id, .. }
            if id == "4242" && kind == "shell" && label == "npm run dev"
                && tool_use_id.as_deref() == Some("c1"))
        );
        assert!(matches!(b[1], Event::BgStarted { id, .. } if id == "77"));
        // A batch: one command finished, two still run.
        assert!(matches!(b[2], Event::BgStarted { id, .. } if id == "80"));
        assert!(matches!(b[3], Event::BgStarted { id, .. } if id == "81"));
        assert!(
            matches!(b[4], Event::BgEnded { id, status, .. } if id == "4242" && status == "completed")
        );
    }
}
