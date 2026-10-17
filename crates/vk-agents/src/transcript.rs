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

/// A Codex follow-up question that waits while its turn continues. The id is the same JSON
/// tuple Codex uses in its question replies, not the id of the already-completed tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingQuestion {
    pub id: String,
    pub message_id: String,
    pub title: String,
    pub options: Vec<String>,
    pub ts_ms: Option<i64>,
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

/// A message the user typed while the harness worked, which waits in the harness's own queue
/// (Claude `queue-operation` lines). Background-task notices are not listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedInput {
    pub text: String,
    pub ts_ms: Option<i64>,
}

/// Cap on remembered queue entries (a harness that never writes a dequeue cannot grow memory).
const QUEUE_MAX: usize = 64;

/// Cap on remembered tool inputs (for background labels).
const INPUTS_MAX: usize = 4000;
/// The agent path of a Codex thread the user started.
const CODEX_ROOT: &str = "/root";

/// A Codex sub-agent (multi-agent `spawn_agent`), keyed by its agent path (`/root/name`) or,
/// for the older tool set, its agent id.
#[derive(Debug, Default)]
struct CodexAgent {
    /// The id its first run is reported under: the child thread id when known, else the key.
    base: String,
    /// The id of the current (or last) run: `base`, then `base#2`, `base#3` for later runs
    /// (`followup_task`), so an end of one run never hides the start of the next.
    id: String,
    runs: u32,
    running: bool,
    started_ms: Option<i64>,
    /// An end arrived before anything named the agent as started: its start is not reported.
    ended_early: bool,
}

/// A Codex collaboration tool call that starts, restarts, ends or lists sub-agents.
#[derive(Debug, Clone)]
struct CodexCall {
    tool: String,
    /// `namespace: collaboration` (agents addressed by path) rather than the older tool set
    /// (agents addressed by id).
    v2: bool,
    /// The agent it addresses (a path for `v2`).
    target: Option<String>,
    /// Older tool set: agent ids a wait addresses.
    targets: Vec<String>,
    label: Option<String>,
}

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
    /// This Codex thread's agent path (`/root`, or the one its `session_meta` names).
    codex_self: String,
    codex_agents: HashMap<String, CodexAgent>,
    /// Collaboration calls that already started a child run: activity items and the call's
    /// output both report a spawn, in either order.
    codex_start_calls: HashSet<String>,
    codex_calls: HashMap<String, CodexCall>,
    /// The rollout has `SubAgentActivity` items, which say when sub-agents start and end;
    /// inter-agent messages are then not used as ends.
    codex_activity: bool,
    codex_questions: Vec<PendingQuestion>,
    codex_question_messages: HashSet<String>,
    codex_question_answers: HashMap<String, String>,
    activity: Option<Activity>,
    totals: TurnUsage,
    /// A title the user gave the session (Claude `/rename`, pi/omp `/name`); wins over `auto_title`.
    user_title: Option<String>,
    /// A title the harness generated (Claude `ai-title`, omp `title`).
    auto_title: Option<String>,
    /// The first prompt, the fallback title.
    first_prompt: Option<String>,
    /// Claude's input queue as its `queue-operation` lines describe it, oldest first. Task
    /// notices stay in it so a `dequeue` (which names no content) pops the right entry.
    queue: Vec<QueuedInput>,
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
            codex_self: CODEX_ROOT.to_string(),
            codex_agents: HashMap::new(),
            codex_start_calls: HashSet::new(),
            codex_calls: HashMap::new(),
            codex_activity: false,
            codex_questions: vec![],
            codex_question_messages: HashSet::new(),
            codex_question_answers: HashMap::new(),
            activity: None,
            totals: TurnUsage::default(),
            user_title: None,
            auto_title: None,
            first_prompt: None,
            queue: vec![],
            skipped: 0,
        }
    }

    pub fn format(&self) -> Format {
        self.format
    }

    pub fn activity(&self) -> Option<Activity> {
        self.activity
    }

    pub fn pending_questions(&self) -> &[PendingQuestion] {
        &self.codex_questions
    }

    pub fn question_answered(&self, id: &str) -> bool {
        self.codex_question_answers.contains_key(id)
    }

    pub fn question_answer(&self, id: &str) -> Option<&str> {
        self.codex_question_answers.get(id).map(String::as_str)
    }

    pub fn question_answers(&self) -> &HashMap<String, String> {
        &self.codex_question_answers
    }

    pub fn at_record_boundary(&self) -> bool {
        self.carry.is_empty()
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

    /// Messages waiting in the harness's queue (user input only), oldest first.
    pub fn queued_inputs(&self) -> Vec<QueuedInput> {
        self.queue
            .iter()
            .filter(|q| !q.text.trim_start().starts_with("<task-notification>"))
            .cloned()
            .collect()
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
        // Codex can write a command's end before the output that names its session.
        if id.is_empty()
            || self.seen_bg_end.contains(id)
            || !self.seen_bg_start.insert(id.to_string())
        {
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

    /// A Claude `<task-notification>` with a status ends that task.
    fn claude_task_ended(&mut self, text: &str, ts: Option<i64>, out: &mut Vec<Event>) {
        if let Some(n) = parse_task_notification(text)
            && let Some(status) = n.status
        {
            self.bg_ended(&n.task_id, &status, ts, out);
        }
    }

    /// Background records of a Claude `user` line.
    fn claude_bg(&mut self, v: &Value, ts: Option<i64>, out: &mut Vec<Event>) {
        let content = v.pointer("/message/content");
        let notification = v.pointer("/origin/kind").and_then(Value::as_str)
            == Some("task-notification")
            || user_text(content)
                .is_some_and(|t| t.trim_start().starts_with("<task-notification>"));
        if notification {
            if let Some(t) = user_text(content) {
                self.claude_task_ended(&t, ts, out);
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

    /// Claude's queue: `enqueue` (content) appends; `dequeue` pops the front (the next turn
    /// takes it); `remove` (content, e.g. `absorbed_mid_turn` or `delivered_to_agent`) drops that
    /// entry; `popAll` moves every entry back into the input box.
    fn claude_queue_op(&mut self, v: &Value, ts: Option<i64>) {
        let content = user_text(v.get("content"));
        match v.get("operation").and_then(Value::as_str) {
            Some("enqueue") => {
                if let Some(text) = content {
                    if self.queue.len() >= QUEUE_MAX {
                        self.queue.remove(0);
                    }
                    self.queue.push(QueuedInput { text, ts_ms: ts });
                }
            }
            Some("dequeue") => {
                if !self.queue.is_empty() {
                    self.queue.remove(0);
                }
            }
            Some("remove") => match content {
                Some(text) => {
                    if let Some(i) = self.queue.iter().position(|q| q.text == text) {
                        self.queue.remove(i);
                    }
                }
                None => {
                    if !self.queue.is_empty() {
                        self.queue.remove(0);
                    }
                }
            },
            Some("popAll" | "pop_all" | "clear") => self.queue.clear(),
            _ => {}
        }
    }

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
            // A task notification that arrives while a turn runs is delivered as an attachment
            // to that turn, not as a user line. Its queue entry is written when the task ends.
            Some("attachment") if !sidechain => {
                let a = &v["attachment"];
                if a.get("type").and_then(Value::as_str) == Some("queued_command")
                    && let Some(p) = a.get("prompt").and_then(Value::as_str)
                {
                    self.claude_task_ended(p, ts, out);
                }
            }
            Some("queue-operation") => {
                if v.get("operation").and_then(Value::as_str) == Some("enqueue")
                    && let Some(c) = v.get("content").and_then(Value::as_str)
                {
                    self.claude_task_ended(c, ts, out);
                }
                if !sidechain {
                    self.claude_queue_op(v, ts);
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
            Some("session_meta") => {
                // A sub-agent's own rollout names its path; relative targets resolve below it.
                if let Some(path) = p
                    .get("agent_path")
                    .and_then(Value::as_str)
                    .filter(|a| a.starts_with('/'))
                {
                    self.codex_self = path.to_string();
                }
            }
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
                        if self.codex_question_replies(m) {
                            return;
                        }
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
                    self.codex_questions.clear();
                    let id = p
                        .get("turn_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("turn{}", self.turns_done + 1));
                    self.finish_turn(id, Some(pty.to_string()), ts, out);
                }
                "item_completed" => {
                    let item = &p["item"];
                    if item.get("type").and_then(Value::as_str) == Some("AgentMessage")
                        && item.get("delivery").and_then(Value::as_str) == Some("async")
                    {
                        self.codex_question_item(item, ts);
                    }
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
                    if item.get("type").and_then(Value::as_str) == Some("SubAgentActivity") {
                        self.codex_subagent_activity(item, ts, out);
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
                "message" if p.get("role").and_then(Value::as_str) == Some("user") => {
                    if let Some(content) = p.get("content").and_then(Value::as_array) {
                        for block in content {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                self.codex_question_replies(text);
                            }
                        }
                    }
                }
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
                    if pty == "function_call" {
                        let ns = p.get("namespace").and_then(Value::as_str);
                        self.codex_collab_call(id, name, ns, &args);
                    }
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
                    // Kept after its output: a `SubAgentActivity` item may still name the call.
                    if let Some(call) = self.codex_calls.get(id).cloned() {
                        self.codex_collab_output(id, call, p.get("output"), ts, out);
                    }
                }
                "agent_message" => self.codex_agent_message(p, ts, out),
                _ => {}
            },
            _ => {}
        }
    }

    fn codex_question_item(&mut self, item: &Value, ts_ms: Option<i64>) {
        let Some(id) = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let Some(questions) = item.get("questions").and_then(Value::as_array) else {
            return;
        };
        if !self.codex_question_messages.insert(id.to_string()) {
            return;
        }
        for (index, q) in questions.iter().take(32).enumerate() {
            let Some(title) = q.get("title").and_then(Value::as_str) else {
                continue;
            };
            if self.codex_questions.len() >= INPUTS_MAX {
                break;
            }
            self.codex_questions.push(PendingQuestion {
                id: serde_json::json!(["request_user_input_async", id, index]).to_string(),
                message_id: id.to_string(),
                title: title.to_string(),
                options: q
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .take(32)
                    .filter(|s| s.len() <= 512)
                    .map(str::to_string)
                    .collect(),
                ts_ms,
            });
        }
    }

    fn codex_question_replies(&mut self, text: &str) -> bool {
        // Only a committed user message can retire a question. Tool output saying
        // {"accepted":true} acknowledges enqueueing the question, not answering it.
        let Some(body) = tag(text, "send_user_message_question_reply") else {
            return false;
        };
        let Ok(Value::Array(replies)) = serde_json::from_str::<Value>(body) else {
            return false;
        };
        // A queued reply can be written after task_complete cleared the visible questions.
        for reply in &replies {
            if let (Some(id), Some(answer)) = (
                reply.get("questionItemId").and_then(Value::as_str),
                reply.get("answer").and_then(Value::as_str),
            ) {
                self.codex_question_answers
                    .insert(id.to_string(), answer.to_string());
            }
        }
        self.codex_questions.retain(|q| {
            let answer = replies
                .iter()
                .find(|r| {
                    r.get("questionItemId")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id == q.id || id == q.message_id)
                })
                .and_then(|r| r.get("answer"))
                .and_then(Value::as_str);
            if let Some(answer) = answer {
                self.codex_question_answers
                    .insert(q.id.clone(), answer.to_string());
            }
            answer.is_none()
        });
        true
    }

    // ---- Codex sub-agents ------------------------------------------------------------------
    //
    // Codex runs a sub-agent as a child thread of the same process. `spawn_agent` returns at
    // once while the child works, so every sub-agent is background work until it ends.
    //
    // Current tool set (`namespace: collaboration`): `spawn_agent {task_name}` answers
    // `{"task_name": "/root/name"}`; `followup_task {target}` gives a finished agent a new task;
    // `interrupt_agent {target}` answers `{"previous_status"}`; `list_agents` answers
    // `{"agents": [{agent_name, agent_status}]}`. Newer versions also write `event_msg`
    // `item_completed` items `{type: SubAgentActivity, id: <call id>, kind: started |
    // interacted | completed | interrupted, agent_thread_id, agent_path}`. Without them, the
    // child's `agent_message` to its parent with `Message Type: FINAL_ANSWER` ends it.
    //
    // Older tool set (no namespace): `spawn_agent` answers `{agent_id, nickname}`,
    // `send_input {id}` starts a turn, `wait_agent {targets}` answers `{status: {id: status}}`,
    // `close_agent {target}` ends it.

    /// The path a collaboration target names: relative names resolve below this thread.
    fn codex_agent_path(&self, target: &str) -> String {
        if target.starts_with('/') {
            target.to_string()
        } else {
            format!("{}/{}", self.codex_self.trim_end_matches('/'), target)
        }
    }

    /// Remember a collaboration call that starts, restarts, ends or lists sub-agents.
    fn codex_collab_call(&mut self, id: &str, name: &str, ns: Option<&str>, args: &Value) {
        let v2 = ns == Some("collaboration");
        let older = ns.is_none()
            && (matches!(
                name,
                "spawn_agent" | "send_input" | "wait_agent" | "close_agent"
            ) || (name == "wait" && args.get("ids").is_some_and(Value::is_array)));
        let tracked = matches!(
            name,
            "spawn_agent" | "followup_task" | "interrupt_agent" | "list_agents"
        );
        if id.is_empty() || !((v2 && tracked) || older) {
            return;
        }
        let s = |k: &str| {
            args.get(k)
                .and_then(Value::as_str)
                .filter(|v| !v.trim().is_empty())
        };
        let target = s("target").or_else(|| s("id")).map(|t| {
            if v2 {
                self.codex_agent_path(t)
            } else {
                t.to_string()
            }
        });
        let targets = args
            .get("targets")
            .or_else(|| args.get("ids"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let label = (name == "spawn_agent")
            .then(|| codex_spawn_label(args))
            .flatten();
        if self.codex_calls.len() >= INPUTS_MAX {
            self.codex_calls.clear();
        }
        self.codex_calls.insert(
            id.to_string(),
            CodexCall {
                tool: name.to_string(),
                v2,
                target,
                targets,
                label,
            },
        );
    }

    /// The output of a remembered collaboration call.
    fn codex_collab_output(
        &mut self,
        call_id: &str,
        call: CodexCall,
        output: Option<&Value>,
        ts: Option<i64>,
        out: &mut Vec<Event>,
    ) {
        let text = output_text(output);
        // `collab tool failed: …` (a thread limit, an unknown target): nothing changed.
        let failed = text.trim_start().starts_with("collab tool failed");
        let o: Value = serde_json::from_str(text.trim()).unwrap_or(Value::Null);
        let s = |k: &str| o.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
        match (call.tool.as_str(), call.v2) {
            ("spawn_agent", true) => {
                if let Some(path) = s("task_name") {
                    let path = self.codex_agent_path(path);
                    self.codex_agent_start(&path, None, Some(call_id), call.label, ts, out);
                }
            }
            ("spawn_agent", false) => {
                if let Some(agent) = s("agent_id") {
                    let label = call.label.or_else(|| s("nickname").map(str::to_string));
                    let agent = agent.to_string();
                    self.codex_agent_start(&agent, Some(&agent), Some(call_id), label, ts, out);
                }
            }
            // A new task (or input) starts a turn of the child. With activity items, their
            // `interacted` entry says so (and this output may follow the child's end).
            ("followup_task", true) | ("send_input", false) => {
                if !failed
                    && !self.codex_activity
                    && let Some(t) = call.target.as_deref()
                {
                    self.codex_agent_start(t, None, Some(call_id), None, ts, out);
                }
            }
            ("interrupt_agent", true) => {
                if !failed && let Some(t) = call.target.as_deref() {
                    self.codex_agent_end(t, "interrupted", ts, out);
                }
            }
            ("close_agent", false) => {
                if !failed && let Some(t) = call.target.as_deref() {
                    self.codex_agent_end(t, "closed", ts, out);
                }
            }
            // A snapshot of every agent: ends the ones that no longer run.
            ("list_agents", true) => {
                for a in o
                    .get("agents")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let (Some(name), Some(status)) = (
                        a.get("agent_name").and_then(Value::as_str),
                        a.get("agent_status").and_then(status_word),
                    ) else {
                        continue;
                    };
                    let name = self.codex_agent_path(name);
                    if !codex_status_running(&status)
                        && self.codex_agents.get(&name).is_some_and(|g| g.running)
                    {
                        self.codex_agent_end(&name, &status, ts, out);
                    }
                }
            }
            ("wait_agent" | "wait", false) => {
                let Some(m) = o.get("status").and_then(Value::as_object) else {
                    return;
                };
                for (agent, st) in m {
                    if let Some(status) = status_word(st)
                        && !codex_status_running(&status)
                        && (call.targets.is_empty() || call.targets.contains(agent))
                    {
                        self.codex_agent_end(agent, &status, ts, out);
                    }
                }
            }
            _ => {}
        }
    }

    /// `SubAgentActivity`: the harness's own record of a sub-agent starting, getting a new
    /// task, and ending.
    fn codex_subagent_activity(&mut self, item: &Value, ts: Option<i64>, out: &mut Vec<Event>) {
        let s = |k: &str| {
            item.get(k)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let thread = s("agent_thread_id");
        let Some(key) = s("agent_path")
            .map(|p| self.codex_agent_path(&p))
            .or_else(|| thread.clone())
        else {
            return;
        };
        self.codex_activity = true;
        let call_id = s("id");
        let call = call_id
            .as_deref()
            .and_then(|c| self.codex_calls.get(c))
            .cloned();
        match s("kind").as_deref().unwrap_or("") {
            "started" => {
                let label = call.and_then(|c| c.label);
                self.codex_agent_start(&key, thread.as_deref(), call_id.as_deref(), label, ts, out);
            }
            // `send_message` also interacts, but only `followup_task` starts a turn.
            "interacted"
                if call
                    .as_ref()
                    .is_some_and(|c| matches!(c.tool.as_str(), "followup_task" | "send_input")) =>
            {
                self.codex_agent_start(&key, thread.as_deref(), call_id.as_deref(), None, ts, out);
            }
            kind @ ("completed" | "interrupted" | "errored" | "failed" | "shutdown" | "closed") => {
                self.codex_agent_end(&key, kind, ts, out);
            }
            _ => {}
        }
    }

    /// A child's final answer also ends its run when its completion activity was not written.
    fn codex_agent_message(&mut self, p: &Value, ts: Option<i64>, out: &mut Vec<Event>) {
        if p.get("recipient").and_then(Value::as_str) != Some(self.codex_self.as_str()) {
            return;
        }
        let Some(author) = p
            .get("author")
            .and_then(Value::as_str)
            .filter(|a| a.starts_with('/') && *a != self.codex_self)
        else {
            return;
        };
        let head = p
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("");
        if head.starts_with("Message Type: FINAL_ANSWER") {
            // The parent may receive a previous task's final answer after starting another.
            // Current Codex records when the message was created, independently of delivery.
            let created = p
                .pointer("/internal_chat_message_metadata_passthrough/create_time")
                .and_then(Value::as_f64)
                .filter(|t| t.is_finite())
                .map(|t| (t * 1000.0).round() as i64);
            if let Some(a) = self.codex_agents.get(author)
                && (a
                    .started_ms
                    .zip(created)
                    .is_some_and(|(start, end)| end < start)
                    || (self.codex_activity
                        && a.runs > 1
                        && (created.is_none() || a.started_ms.is_none())))
            {
                return;
            }
            let author = author.to_string();
            self.codex_agent_end(&author, "completed", created.or(ts), out);
        }
    }

    fn codex_agent_start(
        &mut self,
        key: &str,
        thread: Option<&str>,
        call_id: Option<&str>,
        label: Option<String>,
        ts: Option<i64>,
        out: &mut Vec<Event>,
    ) {
        if let Some(c) = call_id
            && !self.codex_start_calls.insert(c.to_string())
        {
            return;
        }
        let a = self.codex_agents.entry(key.to_string()).or_default();
        if a.base.is_empty() {
            a.base = thread.unwrap_or(key).to_string();
        }
        if a.running {
            return;
        }
        if a.ended_early {
            a.ended_early = false;
            a.runs += 1;
            return;
        }
        a.runs += 1;
        a.running = true;
        a.started_ms = ts;
        a.id = if a.runs == 1 {
            a.base.clone()
        } else {
            format!("{}#{}", a.base, a.runs)
        };
        let id = a.id.clone();
        let label = label.or_else(|| {
            key.rsplit('/')
                .next()
                .filter(|n| !n.is_empty())
                .map(str::to_string)
        });
        self.bg_started(&id, "subagent", call_id, label, ts, out);
    }

    fn codex_agent_end(&mut self, key: &str, status: &str, ts: Option<i64>, out: &mut Vec<Event>) {
        let a = self.codex_agents.entry(key.to_string()).or_default();
        if a.running {
            a.running = false;
            let id = a.id.clone();
            self.bg_ended(&id, status, ts, out);
        } else if a.runs == 0 {
            // Written before the record that starts it: that start is not reported.
            a.base = key.to_string();
            a.ended_early = true;
        }
    }

    // ---- pi / omp --------------------------------------------------------------------------

    fn pi(&mut self, v: &Value, out: &mut Vec<Event>) {
        let ts = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms);
        match v.get("type").and_then(Value::as_str) {
            Some("message") => {}
            // omp: the process ended (`session_exit`); its async jobs ran inside it.
            Some("custom") => {
                if v.get("customType").and_then(Value::as_str) == Some("session_exit") {
                    self.omp_jobs_exited(ts, out);
                }
                return;
            }
            // omp: finished async jobs delivered to the agent.
            Some("custom_message") => {
                if v.get("customType").and_then(Value::as_str) == Some("async-result") {
                    for j in v
                        .pointer("/details/jobs")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if let Some(id) = j.get("jobId").and_then(Value::as_str) {
                            self.bg_ended(id, "completed", ts, out);
                        }
                    }
                }
                return;
            }
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
                self.omp_jobs(m, ts, out);
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
                            self.remember_input(tid, cmd.as_deref());
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

    /// omp async jobs in a tool result: a start (`details.async.state == "running"`; a `task`
    /// call starts one job per pending spawn in `details.progress`), and ends in `hub`
    /// `wait`/`jobs` snapshots (`details.jobs[]` with a final status).
    fn omp_jobs(&mut self, m: &Value, ts: Option<i64>, out: &mut Vec<Event>) {
        let d = &m["details"];
        let call = m.get("toolCallId").and_then(Value::as_str);
        if let Some(a) = d.get("async")
            && a.get("state").and_then(Value::as_str) == Some("running")
        {
            let ty = a.get("type").and_then(Value::as_str).unwrap_or("task");
            let kind = match ty {
                "bash" => "shell",
                "task" => "subagent",
                other => other,
            };
            let mut ids: Vec<&str> = a.get("jobId").and_then(Value::as_str).into_iter().collect();
            if ty == "task" {
                for p in d
                    .get("progress")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if matches!(
                        p.get("status").and_then(Value::as_str),
                        Some("pending" | "running")
                    ) && let Some(id) = p.get("id").and_then(Value::as_str)
                        && !ids.contains(&id)
                    {
                        ids.push(id);
                    }
                }
            }
            for id in ids {
                // A task job is labelled with its agent id (as omp does); a shell with its command.
                let label = (ty == "task").then(|| id.to_string());
                self.omp_job_started(id, kind, call, label, ts, out);
            }
        }
        if m.get("toolName").and_then(Value::as_str) == Some("hub") {
            for j in d
                .get("jobs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let (Some(id), Some(st @ ("completed" | "failed" | "cancelled"))) = (
                    j.get("id").and_then(Value::as_str),
                    j.get("status").and_then(Value::as_str),
                ) {
                    self.bg_ended(id, st, ts, out);
                }
            }
        }
    }

    /// omp job ids are unique within one process only: a start of an id that already ended is
    /// a new job (the session was resumed).
    fn omp_job_started(
        &mut self,
        id: &str,
        kind: &str,
        call: Option<&str>,
        label: Option<String>,
        ts: Option<i64>,
        out: &mut Vec<Event>,
    ) {
        if self.seen_bg_end.remove(id) {
            self.seen_bg_start.remove(id);
        }
        self.bg_started(id, kind, call, label, ts, out);
    }

    /// The omp process ended: every job still open ended with it.
    fn omp_jobs_exited(&mut self, ts: Option<i64>, out: &mut Vec<Event>) {
        let mut open: Vec<String> = self
            .seen_bg_start
            .iter()
            .filter(|id| !self.seen_bg_end.contains(*id))
            .cloned()
            .collect();
        open.sort();
        for id in open {
            self.bg_ended(&id, "stopped", ts, out);
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

/// A tool output as text: a string, or its text blocks joined.
fn output_text(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A Codex sub-agent's label from its `spawn_agent` arguments: the task name (and agent
/// type). The task text itself is often encrypted (`gAAAA…`) and then not used.
fn codex_spawn_label(args: &Value) -> Option<String> {
    let s = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let task = s("task_name").or_else(|| s("message").filter(|m| !m.starts_with("gAAAA")));
    match (task, s("agent_type")) {
        (Some(t), Some(ty)) => Some(format!("{t} ({ty})")),
        (Some(t), None) => Some(t.to_string()),
        (None, ty) => ty.map(str::to_string),
    }
}

/// A Codex agent status as a word: `"running"`, or the key of `{"completed": "…"}`.
fn status_word(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => m.keys().next().cloned(),
        _ => None,
    }
}

fn codex_status_running(status: &str) -> bool {
    matches!(status, "running" | "pending_init" | "pendingInit")
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
    fn codex_follow_up_questions_outlive_the_tool_ack_and_other_work() {
        let mut p = Parser::new(Format::CodexRollout);
        let item = serde_json::json!({"timestamp":"2026-10-16T10:00:00Z", "type":"event_msg", "payload": {
            "type":"item_completed", "item":{"type":"AgentMessage", "id":"call_q", "delivery":"async",
                "questions":[{"title":"Which environment?", "options":["Test", "Production"]}, {"title":"Any constraints?"}]}}});
        p.feed(&lines(&[item.clone(), item.clone(),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call_q","output":"{\"accepted\":true}"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"function_call","call_id":"work","name":"exec_command","arguments":"{}"}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"Please keep going"}}),
        ]));
        assert_eq!(p.pending_questions().len(), 2);
        assert_eq!(
            p.pending_questions()[0].id,
            r#"["request_user_input_async","call_q",0]"#
        );
        assert_eq!(p.pending_questions()[0].options, ["Test", "Production"]);
        assert!(p.pending_questions()[1].options.is_empty());
        assert_eq!(p.activity(), Some(Activity::Working));
        let reply =
            serde_json::json!([{"questionItemId":p.pending_questions()[0].id,"answer":"Test"}]);
        let text = format!(
            "<send_user_message_question_reply>\n{reply}\n</send_user_message_question_reply>"
        );
        p.feed(&lines(&[serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}}),item]));
        assert_eq!(
            p.pending_questions().len(),
            1,
            "replay must not restore an answered question"
        );
        assert!(p.question_answered(r#"["request_user_input_async","call_q",0]"#));
        assert!(!p.question_answered(r#"["request_user_input_async","call_q",1]"#));
        p.feed(&lines(&[serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"t1"}})]));
        assert!(p.pending_questions().is_empty());
    }

    #[test]
    fn codex_follow_up_questions_require_a_completed_async_message() {
        let mut p = Parser::new(Format::CodexRollout);
        for (event, delivery) in [("item_started", "async"), ("item_completed", "sync")] {
            p.feed(&lines(&[
                serde_json::json!({"type":"event_msg", "payload": {
                "type":event, "item":{"type":"AgentMessage", "id":"q", "delivery":delivery,
                "questions":[{"title":"Pick?", "options":["One"]}]}}}),
            ]));
        }
        assert!(p.pending_questions().is_empty());
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

    /// omp async jobs, in the shapes omp writes (anonymised from real session files).
    #[test]
    fn omp_async_jobs_start_and_end() {
        let mut p = Parser::new(Format::OmpJsonl);
        let ev = p.feed(&lines(&[
            // `bash` with async on: the result says the job runs on.
            json!({"type":"message","id":"a1","timestamp":"2026-08-11T12:10:00Z","message":{"role":"assistant","stopReason":"toolUse","content":[
                {"type":"toolCall","id":"call_b1|fc_1","name":"bash","arguments":{"command":"pnpm --filter app ts-check","async":true}}]}}),
            json!({"type":"message","id":"r1","timestamp":"2026-08-11T12:10:01Z","message":{"role":"toolResult","toolCallId":"call_b1|fc_1","toolName":"bash",
                "details":{"async":{"state":"running","jobId":"bg_1","type":"bash"},"timeoutSeconds":600},
                "content":[{"type":"text","text":"Background job bg_1 started"}]}}),
            // `task` spawning two background agents: one job per pending spawn.
            json!({"type":"message","id":"r2","timestamp":"2026-08-11T12:10:02Z","message":{"role":"toolResult","toolCallId":"call_t1|fc_2","toolName":"task",
                "details":{"projectAgentsDir":null,"results":[],"totalDurationMs":4,
                    "progress":[{"index":0,"id":"PatternsScout","agent":"scout","status":"pending","task":"..."},
                                {"index":1,"id":"TestsScout","agent":"scout","status":"pending","task":"..."}],
                    "async":{"state":"running","jobId":"PatternsScout","type":"task"}},
                "content":[{"type":"text","text":"Spawned 2 background agents using scout."}]}}),
            // The result of a finished job is delivered to the agent.
            json!({"type":"custom_message","customType":"async-result","display":true,"timestamp":"2026-08-11T12:11:00Z",
                "details":{"jobs":[{"jobId":"bg_1","type":"bash","label":"pnpm --filter app ts-check","durationMs":59000}]},
                "content":"<system-notice>Background job bg_1 has completed."}),
            // Or a `hub` wait snapshot consumes it first.
            json!({"type":"message","id":"r3","timestamp":"2026-08-11T12:12:00Z","message":{"role":"toolResult","toolCallId":"call_h1|fc_3","toolName":"hub",
                "details":{"op":"wait","jobs":[{"id":"PatternsScout","type":"task","status":"completed","label":"PatternsScout"},
                                              {"id":"TestsScout","type":"task","status":"running","label":"TestsScout"}]}}}),
        ]));
        let b = bg(&ev);
        assert_eq!(b.len(), 5, "{b:?}");
        assert!(
            matches!(b[0], Event::BgStarted { id, kind, label, tool_use_id, ts_ms }
                if id == "bg_1" && kind == "shell" && label == "pnpm --filter app ts-check"
                    && tool_use_id.as_deref() == Some("call_b1|fc_1") && ts_ms.is_some())
        );
        assert!(
            matches!(b[1], Event::BgStarted { id, kind, label, tool_use_id, .. }
                if id == "PatternsScout" && kind == "subagent" && label == "PatternsScout"
                    && tool_use_id.as_deref() == Some("call_t1|fc_2"))
        );
        assert!(
            matches!(b[2], Event::BgStarted { id, kind, .. } if id == "TestsScout" && kind == "subagent")
        );
        assert!(
            matches!(b[3], Event::BgEnded { id, status, .. } if id == "bg_1" && status == "completed")
        );
        assert!(
            matches!(b[4], Event::BgEnded { id, status, .. } if id == "PatternsScout" && status == "completed")
        );
    }

    #[test]
    fn omp_jobs_end_with_the_process_and_ids_are_reused_after_a_resume() {
        let mut p = Parser::new(Format::OmpJsonl);
        let start = |id: &str, call: &str, t: &str| {
            json!({"type":"message","timestamp":t,"message":{"role":"toolResult","toolCallId":call,"toolName":"bash",
                "details":{"async":{"state":"running","jobId":id,"type":"bash"}}}})
        };
        let ev = p.feed(&lines(&[
            start("bg_1", "c1", "2026-08-11T12:00:00Z"),
            start("bg_2", "c2", "2026-08-11T12:00:01Z"),
            json!({"type":"custom_message","customType":"async-result","timestamp":"2026-08-11T12:00:05Z","details":{"jobs":[{"jobId":"bg_1","type":"bash"}]}}),
            // The process exits; bg_2 died with it.
            json!({"type":"custom","customType":"session_exit","timestamp":"2026-08-11T12:01:00Z","data":{"reason":"dispose","kind":"normal"}}),
            // A resumed process numbers its jobs from bg_1 again.
            start("bg_1", "c3", "2026-08-11T13:00:00Z"),
            json!({"type":"custom_message","customType":"async-result","timestamp":"2026-08-11T13:00:05Z","details":{"jobs":[{"jobId":"bg_1","type":"bash"}]}}),
        ]));
        let b = bg(&ev);
        assert_eq!(b.len(), 6, "{b:?}");
        assert!(matches!(b[5], Event::BgEnded { id, .. } if id == "bg_1"));
        assert!(matches!(b[2], Event::BgEnded { id, .. } if id == "bg_1"));
        assert!(
            matches!(b[3], Event::BgEnded { id, status, .. } if id == "bg_2" && status == "stopped")
        );
        assert!(
            matches!(b[4], Event::BgStarted { id, tool_use_id, .. } if id == "bg_1" && tool_use_id.as_deref() == Some("c3"))
        );
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

    #[test]
    fn claude_task_notifications_delivered_mid_turn_end_tasks() {
        let text = |id: &str| {
            format!(
                "<task-notification>\n<task-id>{id}</task-id>\n<status>completed</status>\n<summary>done</summary>\n</task-notification>"
            )
        };
        let mut p = Parser::new(Format::ClaudeJsonl);
        let ev = p.feed(&lines(&[
            json!({"type":"user","timestamp":"2026-01-01T00:00:01Z",
                "toolUseResult":{"stdout":"","backgroundTaskId":"bg1"},
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu1","content":"running"}]}}),
            json!({"type":"user","timestamp":"2026-01-01T00:00:02Z",
                "toolUseResult":{"status":"async_launched","agentId":"ag1","description":"Map it"},
                "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu2","content":"launched"}]}}),
            // The task ends while a turn runs: a queue entry, then an attachment to that turn.
            json!({"type":"queue-operation","operation":"enqueue","timestamp":"2026-01-01T00:00:03Z","content":text("bg1")}),
            json!({"type":"attachment","isSidechain":false,"timestamp":"2026-01-01T00:00:04Z",
                "attachment":{"type":"queued_command","commandMode":"task-notification","prompt":text("bg1")}}),
            // Only an attachment (an older harness version writes no queue entry).
            json!({"type":"attachment","isSidechain":false,"timestamp":"2026-01-01T00:00:05Z",
                "attachment":{"type":"queued_command","commandMode":"task-notification","prompt":text("ag1")}}),
            // A queued user prompt is not a task end.
            json!({"type":"queue-operation","operation":"enqueue","timestamp":"2026-01-01T00:00:06Z","content":"also fix the tests"}),
        ]));
        let b = bg(&ev);
        assert_eq!(b.len(), 4, "{b:?}");
        assert!(
            matches!(b[2], Event::BgEnded { id, status, .. } if id == "bg1" && status == "completed")
        );
        assert!(matches!(b[3], Event::BgEnded { id, .. } if id == "ag1"));
    }

    #[test]
    fn claude_queue_follows_queue_operations() {
        let note = "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>";
        let op = |op: &str, content: Option<&str>, t: u32| {
            let mut v = json!({"type":"queue-operation","operation":op,"timestamp":format!("2026-01-01T00:00:{t:02}Z"),"sessionId":"s"});
            if let Some(c) = content {
                v["content"] = json!(c);
            }
            v
        };
        let mut p = Parser::new(Format::ClaudeJsonl);
        p.feed(&lines(&[
            op("enqueue", Some(note), 1),
            op("enqueue", Some("first"), 2),
            op("enqueue", Some("second"), 3),
        ]));
        let q = p.queued_inputs();
        assert_eq!(q.len(), 2, "task notices are not listed: {q:?}");
        assert_eq!(q[0].text, "first");
        assert_eq!(q[0].ts_ms, parse_rfc3339_ms("2026-01-01T00:00:02Z"));
        // The dequeue pops the notice (the front), not a user message.
        p.feed(&lines(&[op("dequeue", None, 4)]));
        assert_eq!(p.queued_inputs().len(), 2);
        // Absorbed mid-turn: removed by content.
        let mut rm = op("remove", Some("second"), 5);
        rm["reason"] = json!("absorbed_mid_turn");
        p.feed(&lines(&[rm]));
        let q = p.queued_inputs();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].text, "first");
        p.feed(&lines(&[op("dequeue", None, 6)]));
        assert!(p.queued_inputs().is_empty());
        // popAll takes everything back into the input box.
        p.feed(&lines(&[
            op("enqueue", Some("a"), 7),
            op("enqueue", Some("b"), 8),
        ]));
        p.feed(&lines(&[op("popAll", Some("a\nb"), 9)]));
        assert!(p.queued_inputs().is_empty());
        // Unknown operations and sidechain lines change nothing.
        let mut side = op("enqueue", Some("sub"), 10);
        side["isSidechain"] = json!(true);
        p.feed(&lines(&[op("peek", Some("x"), 10), side]));
        assert!(p.queued_inputs().is_empty());
    }

    #[test]
    fn codex_end_written_before_the_session_output_is_not_a_start() {
        let mut p = Parser::new(Format::CodexRollout);
        let ev = p.feed(&lines(&[
            json!({"timestamp":"2026-01-01T00:00:01Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","id":"e1","process_id":"81684","status":"completed"}}}),
            json!({"timestamp":"2026-01-01T00:00:02Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c1","output":[{"type":"input_text","text":"{\"chunk_id\":\"a\",\"session_id\":81684,\"output\":\"\"}"},{"type":"input_text","text":"{\"chunk_id\":\"b\",\"exit_code\":0,\"output\":\"\"}"}]}}),
        ]));
        let b = bg(&ev);
        assert_eq!(b.len(), 1, "{b:?}");
        assert!(matches!(b[0], Event::BgEnded { id, .. } if id == "81684"));
    }

    /// Background events as `start <id> <kind> <label> <tool>` / `end <id> <status>`.
    fn bg_brief(ev: &[Event]) -> Vec<String> {
        ev.iter()
            .filter_map(|e| match e {
                Event::BgStarted {
                    id,
                    kind,
                    label,
                    tool_use_id,
                    ..
                } => Some(format!(
                    "start {id} {kind} {label} {}",
                    tool_use_id.as_deref().unwrap_or("-")
                )),
                Event::BgEnded { id, status, .. } => Some(format!("end {id} {status}")),
                _ => None,
            })
            .collect()
    }

    fn codex_call(t: &str, call: &str, name: &str, args: Value) -> Value {
        json!({"timestamp": t, "type": "response_item", "payload": {"type": "function_call",
            "name": name, "namespace": "collaboration", "arguments": args.to_string(), "call_id": call}})
    }

    fn codex_out(t: &str, call: &str, output: &str) -> Value {
        json!({"timestamp": t, "type": "response_item", "payload": {"type": "function_call_output",
            "call_id": call, "output": output}})
    }

    fn codex_activity(t: &str, call: &str, kind: &str, thread: &str, path: &str) -> Value {
        json!({"timestamp": t, "type": "event_msg", "payload": {"type": "item_completed",
            "thread_id": "root-thread", "turn_id": "turn-1",
            "item": {"type": "SubAgentActivity", "model": null, "reasoning_effort": null,
                "id": call, "kind": kind, "agent_thread_id": thread, "agent_path": path}}})
    }

    fn codex_agent_msg(t: &str, kind: &str, from: &str, to: &str) -> Value {
        let head = format!("Message Type: {kind}\nTask name: {to}\nSender: {from}\nPayload:\n");
        json!({"timestamp": t, "type": "response_item", "payload": {"type": "agent_message",
            "id": format!("amsg-{t}"), "author": from, "recipient": to,
            "internal_chat_message_metadata_passthrough": {"create_time": parse_rfc3339_ms(t).unwrap() as f64 / 1000.0},
            "content": [{"type": "input_text", "text": head},
                {"type": "encrypted_content", "encrypted_content": "gAAAAxyz"}]}})
    }

    /// Shapes from codex-cli 0.158–0.162 rollouts (ids and names changed).
    #[test]
    fn codex_subagents_follow_activity_items() {
        let mut p = Parser::new(Format::CodexRollout);
        let ev = p.feed(&lines(&[
            json!({"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"root-thread","source":"cli","thread_source":"user"}}),
            codex_call(
                "2026-01-01T00:00:01Z",
                "c1",
                "spawn_agent",
                json!({"task_name": "client_perf", "message": "gAAAAencrypted"}),
            ),
            codex_activity("2026-01-01T00:00:01Z", "c1", "started", "t1", "/root/client_perf"),
            codex_out("2026-01-01T00:00:01Z", "c1", r#"{"task_name":"/root/client_perf"}"#),
            codex_call(
                "2026-01-01T00:00:02Z",
                "c2",
                "spawn_agent",
                json!({"task_name": "review_login", "agent_type": "code-reviewer", "message": "gAAAAx"}),
            ),
            codex_activity("2026-01-01T00:00:02Z", "c2", "started", "t2", "/root/review_login"),
            codex_out("2026-01-01T00:00:02Z", "c2", r#"{"task_name":"/root/review_login"}"#),
            codex_activity(
                "2026-01-01T00:00:05Z",
                "subagent-completed-x",
                "completed",
                "t2",
                "/root/review_login",
            ),
            // Delivered after the activity item: not a second end.
            codex_agent_msg("2026-01-01T00:00:06Z", "FINAL_ANSWER", "/root/review_login", "/root"),
            // A message does not start a turn of the child.
            codex_call(
                "2026-01-01T00:00:07Z",
                "c3",
                "send_message",
                json!({"target": "review_login", "message": "gAAAAx"}),
            ),
            codex_activity("2026-01-01T00:00:07Z", "c3", "interacted", "t2", "/root/review_login"),
            codex_out("2026-01-01T00:00:07Z", "c3", ""),
            // A follow-up task does: a new run under a new id.
            codex_call(
                "2026-01-01T00:00:08Z",
                "c4",
                "followup_task",
                json!({"target": "review_login", "message": "gAAAAx"}),
            ),
            codex_activity("2026-01-01T00:00:08Z", "c4", "interacted", "t2", "/root/review_login"),
            codex_out("2026-01-01T00:00:08Z", "c4", ""),
            codex_activity(
                "2026-01-01T00:00:09Z",
                "subagent-completed-y",
                "completed",
                "t2",
                "/root/review_login",
            ),
            codex_activity(
                "2026-01-01T00:00:10Z",
                "subagent-completed-z",
                "completed",
                "t1",
                "/root/client_perf",
            ),
        ]));
        assert_eq!(
            bg_brief(&ev),
            [
                "start t1 subagent client_perf c1",
                "start t2 subagent review_login (code-reviewer) c2",
                "end t2 completed",
                "start t2#2 subagent review_login c4",
                "end t2#2 completed",
                "end t1 completed",
            ]
        );
    }

    #[test]
    fn codex_final_answers_close_tasks_when_completion_activity_is_missing() {
        let mut p = Parser::new(Format::CodexRollout);
        let ev = p.feed(&lines(&[
            codex_call(
                "2026-01-01T00:00:01Z",
                "s1",
                "spawn_agent",
                json!({"task_name": "review"}),
            ),
            codex_activity(
                "2026-01-01T00:00:01Z",
                "s1",
                "started",
                "t1",
                "/root/review",
            ),
            codex_out(
                "2026-01-01T00:00:01Z",
                "s1",
                r#"{"task_name":"/root/review"}"#,
            ),
            codex_agent_msg("2026-01-01T00:00:02Z", "MESSAGE", "/root/review", "/root"),
            codex_agent_msg(
                "2026-01-01T00:00:03Z",
                "FINAL_ANSWER",
                "/root/review",
                "/root",
            ),
            // A duplicate or late completion is harmless. A later task still starts normally.
            codex_activity(
                "2026-01-01T00:00:04Z",
                "end1",
                "completed",
                "t1",
                "/root/review",
            ),
            codex_agent_msg(
                "2026-01-01T00:00:04Z",
                "FINAL_ANSWER",
                "/root/review",
                "/root",
            ),
            codex_call(
                "2026-01-01T00:00:05Z",
                "s2",
                "followup_task",
                json!({"target": "review"}),
            ),
            codex_activity(
                "2026-01-01T00:00:05Z",
                "s2",
                "interacted",
                "t1",
                "/root/review",
            ),
            codex_agent_msg(
                "2026-01-01T00:00:06Z",
                "FINAL_ANSWER",
                "/root/review",
                "/root",
            ),
        ]));
        assert_eq!(
            bg_brief(&ev),
            [
                "start t1 subagent review s1",
                "end t1 completed",
                "start t1#2 subagent review s2",
                "end t1#2 completed",
            ]
        );
    }

    #[test]
    fn codex_delayed_final_answer_does_not_end_a_newer_task() {
        let mut p = Parser::new(Format::CodexRollout);
        p.feed(&lines(&[
            codex_call(
                "2026-01-01T00:00:01Z",
                "s1",
                "spawn_agent",
                json!({"task_name":"review"}),
            ),
            codex_activity(
                "2026-01-01T00:00:01Z",
                "s1",
                "started",
                "t1",
                "/root/review",
            ),
            codex_activity(
                "2026-01-01T00:00:03Z",
                "e1",
                "completed",
                "t1",
                "/root/review",
            ),
            codex_call(
                "2026-01-01T00:00:05Z",
                "s2",
                "followup_task",
                json!({"target":"review"}),
            ),
            codex_activity(
                "2026-01-01T00:00:05Z",
                "s2",
                "interacted",
                "t1",
                "/root/review",
            ),
        ]));
        let mut delayed = codex_agent_msg(
            "2026-01-01T00:00:03Z",
            "FINAL_ANSWER",
            "/root/review",
            "/root",
        );
        delayed["timestamp"] = json!("2026-01-01T00:00:06Z");
        let mut ambiguous = codex_agent_msg(
            "2026-01-01T00:00:06Z",
            "FINAL_ANSWER",
            "/root/review",
            "/root",
        );
        ambiguous["payload"]
            .as_object_mut()
            .unwrap()
            .remove("internal_chat_message_metadata_passthrough");
        assert!(bg_brief(&p.feed(&lines(&[delayed, ambiguous]))).is_empty());
        assert_eq!(
            bg_brief(&p.feed(&lines(&[codex_agent_msg(
                "2026-01-01T00:00:07Z",
                "FINAL_ANSWER",
                "/root/review",
                "/root"
            ),]))),
            ["end t1#2 completed"]
        );
    }

    #[test]
    fn codex_subagent_ended_before_its_spawn_output_is_not_started_again() {
        let mut p = Parser::new(Format::CodexRollout);
        let ev = p.feed(&lines(&[
            codex_call(
                "2026-01-01T00:00:01Z",
                "c1",
                "spawn_agent",
                json!({"task_name": "quick", "message": "gAAAAx"}),
            ),
            codex_activity("2026-01-01T00:00:01Z", "c1", "started", "t1", "/root/quick"),
            codex_activity(
                "2026-01-01T00:00:02Z",
                "subagent-completed-q",
                "completed",
                "t1",
                "/root/quick",
            ),
            codex_out(
                "2026-01-01T00:00:03Z",
                "c1",
                r#"{"task_name":"/root/quick"}"#,
            ),
        ]));
        assert_eq!(
            bg_brief(&ev),
            ["start t1 subagent quick c1", "end t1 completed"]
        );
    }

    /// codex-cli 0.146 rollouts: no activity items.
    #[test]
    fn codex_subagents_without_activity_items() {
        let mut p = Parser::new(Format::CodexRollout);
        let ev = p.feed(&lines(&[
            codex_call(
                "2026-01-01T00:00:01Z",
                "c1",
                "spawn_agent",
                json!({"task_name": "design_a11y", "fork_turns": "all", "message": "gAAAAx"}),
            ),
            codex_out("2026-01-01T00:00:01Z", "c1", r#"{"task_name":"/root/design_a11y"}"#),
            codex_call(
                "2026-01-01T00:00:02Z",
                "c2",
                "spawn_agent",
                json!({"task_name": "product_flows", "message": "gAAAAx"}),
            ),
            codex_out("2026-01-01T00:00:02Z", "c2", r#"{"task_name":"/root/product_flows"}"#),
            codex_call("2026-01-01T00:00:03Z", "c3", "list_agents", json!({})),
            codex_out(
                "2026-01-01T00:00:03Z",
                "c3",
                r#"{"agents":[{"agent_name":"/root","agent_status":"running"},{"agent_name":"/root/design_a11y","agent_status":"running"},{"agent_name":"/root/product_flows","agent_status":"running"}]}"#,
            ),
            codex_agent_msg("2026-01-01T00:00:04Z", "MESSAGE", "/root/design_a11y", "/root"),
            codex_agent_msg("2026-01-01T00:00:05Z", "FINAL_ANSWER", "/root/product_flows", "/root"),
            codex_call(
                "2026-01-01T00:00:06Z",
                "c4",
                "interrupt_agent",
                json!({"target": "/root/design_a11y"}),
            ),
            codex_out("2026-01-01T00:00:06Z", "c4", r#"{"previous_status":"running"}"#),
            codex_call(
                "2026-01-01T00:00:07Z",
                "c5",
                "followup_task",
                json!({"target": "/root/design_a11y", "message": "gAAAAx"}),
            ),
            codex_out("2026-01-01T00:00:07Z", "c5", ""),
            codex_call(
                "2026-01-01T00:00:08Z",
                "c6",
                "followup_task",
                json!({"target": "product_flows", "message": "gAAAAx"}),
            ),
            codex_out(
                "2026-01-01T00:00:08Z",
                "c6",
                "collab tool failed: agent thread limit reached",
            ),
            codex_call("2026-01-01T00:00:09Z", "c7", "list_agents", json!({})),
            codex_out(
                "2026-01-01T00:00:09Z",
                "c7",
                r#"{"agents":[{"agent_name":"/root","agent_status":"running"},{"agent_name":"/root/design_a11y","agent_status":"interrupted"},{"agent_name":"/root/product_flows","agent_status":{"completed":"Findings"}}]}"#,
            ),
        ]));
        assert_eq!(
            bg_brief(&ev),
            [
                "start /root/design_a11y subagent design_a11y c1",
                "start /root/product_flows subagent product_flows c2",
                "end /root/product_flows completed",
                "end /root/design_a11y interrupted",
                "start /root/design_a11y#2 subagent design_a11y c5",
                "end /root/design_a11y#2 interrupted",
            ]
        );
    }

    /// The older tool set addresses agents by id (shapes from its tool schemas).
    #[test]
    fn codex_subagents_of_the_older_tool_set() {
        let call = |t: &str, id: &str, name: &str, args: Value| {
            json!({"timestamp": t, "type": "response_item", "payload": {"type": "function_call",
                "name": name, "arguments": args.to_string(), "call_id": id}})
        };
        let mut p = Parser::new(Format::CodexRollout);
        let ev = p.feed(&lines(&[
            call(
                "2026-01-01T00:00:01Z",
                "c1",
                "spawn_agent",
                json!({"message": "Review the parser"}),
            ),
            codex_out(
                "2026-01-01T00:00:01Z",
                "c1",
                r#"{"agent_id":"a1","nickname":"Euler"}"#,
            ),
            // Code mode's `wait` (for a script cell) is not an agent wait.
            call(
                "2026-01-01T00:00:02Z",
                "c2",
                "wait",
                json!({"cell_id": "1", "yield_time_ms": 1000}),
            ),
            codex_out(
                "2026-01-01T00:00:02Z",
                "c2",
                r#"{"status":{"a1":"shutdown"}}"#,
            ),
            call(
                "2026-01-01T00:00:03Z",
                "c3",
                "wait_agent",
                json!({"targets": ["a1"], "timeout_ms": 30000}),
            ),
            codex_out(
                "2026-01-01T00:00:03Z",
                "c3",
                r#"{"status":{"a1":{"completed":"done"}},"timed_out":false}"#,
            ),
            call(
                "2026-01-01T00:00:04Z",
                "c4",
                "send_input",
                json!({"id": "a1", "message": "And the tests"}),
            ),
            codex_out("2026-01-01T00:00:04Z", "c4", r#"{"submission_id":"s1"}"#),
            call(
                "2026-01-01T00:00:05Z",
                "c5",
                "close_agent",
                json!({"target": "a1"}),
            ),
            codex_out(
                "2026-01-01T00:00:05Z",
                "c5",
                r#"{"previous_status":"running"}"#,
            ),
        ]));
        assert_eq!(
            bg_brief(&ev),
            [
                "start a1 subagent Review the parser c1",
                "end a1 completed",
                "start a1#2 subagent a1 c4",
                "end a1#2 closed",
            ]
        );
    }

    /// A sub-agent's own rollout: its children live below its path, and the task its parent
    /// sends it is not an end.
    #[test]
    fn codex_subagent_rollout_resolves_paths_below_its_own() {
        let mut p = Parser::new(Format::CodexRollout);
        let ev = p.feed(&lines(&[
            json!({"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"id":"child-thread",
                "source":{"subagent":{"thread_spawn":{"parent_thread_id":"root-thread","depth":1,"agent_path":"/root/worker","agent_nickname":"Hooke","agent_role":null}}},
                "thread_source":"subagent","parent_thread_id":"root-thread","agent_path":"/root/worker"}}),
            codex_agent_msg("2026-01-01T00:00:01Z", "NEW_TASK", "/root", "/root/worker"),
            codex_call(
                "2026-01-01T00:00:02Z",
                "c1",
                "spawn_agent",
                json!({"task_name": "tests", "message": "gAAAAx"}),
            ),
            codex_out("2026-01-01T00:00:02Z", "c1", r#"{"task_name":"/root/worker/tests"}"#),
            codex_agent_msg("2026-01-01T00:00:03Z", "FINAL_ANSWER", "/root/worker/tests", "/root/worker"),
        ]));
        assert_eq!(
            bg_brief(&ev),
            [
                "start /root/worker/tests subagent tests c1",
                "end /root/worker/tests completed",
            ]
        );
    }
}
