//! Generic ACP agents (04 §6.6): `vibeke acp-host --harness <id> -- <agent argv>` runs in the
//! pane, speaks the Agent Client Protocol (JSON-RPC 2.0, newline-delimited, over the agent's
//! stdio) as the client, renders a minimal transcript in the pane and maps the protocol onto
//! Vibeke signals:
//!
//! | ACP | Vibeke |
//! |---|---|
//! | `session/new` result `sessionId` | `SessionStart` (identity) |
//! | user line → `session/prompt` | `UserPromptSubmit` (turn) |
//! | `session/update` `tool_call` / `tool_call_update` (completed/failed) | `PreToolUse` / `PostToolUse(Failure)` (items) |
//! | `session/update` `agent_message_chunk` | transcript + last message |
//! | `session/prompt` result `stopReason` | `Stop` |
//! | `session/request_permission` | approval Interaction via `adapter.gate`, answered natively with `{outcome: {outcome: "selected", optionId}}` |
//! | Esc / `agent.interrupt` | `session/cancel` |
//! | `fs/read_text_file`, `fs/write_text_file` | served, scoped to the session cwd |
//!
//! The host's own permission prompt in the pane is the "native dialog": with the pane focused
//! (observe mode) or after release-on-focus the user answers there, and remote answers arrive as
//! keystrokes the host parses like a typed choice (the `acp` manifest's screen rules describe
//! the prompt). `terminal/*` is not offered (clientCapabilities.terminal = false).

use super::harness::Harness;
use super::*;
use std::io::{BufRead, BufReader, Read as _, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;

pub const PROTOCOL_VERSION: u64 = 1;

// ---- server side ------------------------------------------------------------------------------

pub(super) fn on_signal(server: &Arc<Server>, pane: &str, h: Harness, event: &str, p: &Value) {
    match event {
        // Answered in the host's own prompt first: native resolution (04 §2.5 rule 2).
        "PermissionResolved" => super::on_signal(server, pane, h, "PermissionDenied", p),
        "Usage" => {
            let run = bound_run(server, pane, h);
            super::usage::from_acp(server, &run, p);
        }
        _ => super::on_signal(server, pane, h, event, p),
    }
}

fn command_of(raw: &Value) -> Option<String> {
    raw.get("command").and_then(|c| {
        c.as_str().map(str::to_string).or_else(|| {
            c.as_array().map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        })
    })
}

/// `session/request_permission {toolCall, options}` → approval Interaction. The ACP options
/// are kept as the interaction's single question (`acp_options`) so the decision can be
/// mapped back to an `optionId`.
pub fn permission_interaction(p: &Value) -> Interaction {
    let tc = p.get("toolCall").cloned().unwrap_or(Value::Null);
    let raw = tc.get("rawInput").cloned().unwrap_or(Value::Null);
    let kind = tc.get("kind").and_then(Value::as_str).unwrap_or("other");
    let title = tc
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("tool call")
        .to_string();
    let command = command_of(&raw);
    let paths: Vec<String> = tc
        .get("locations")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|l| l.get("path").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let tool = match kind {
        "execute" => "Bash",
        "edit" => "Edit",
        "delete" => "Delete",
        "move" => "Move",
        "read" => "Read",
        "fetch" => "WebFetch",
        "search" => "Grep",
        other => other,
    }
    .to_string();
    let (risk, reasons) = harness::risk(&tool, command.as_deref(), &paths);
    let mut it = harness::interaction_from_hook(
        Harness::Claude,
        "PermissionRequest",
        &json!({"tool_name": tool, "tool_input": {}, "tool_use_id": tc.get("toolCallId")}),
    )
    .expect("approval");
    it.title = title.clone();
    it.action = Some(ActionInfo {
        tool,
        summary: title.clone(),
        command,
        paths,
        diff: None,
        risk,
        risk_reasons: reasons,
    });
    it.questions = vec![Question {
        id: "acp_options".into(),
        prompt: title,
        header: None,
        multi: false,
        options: p
            .get("options")
            .and_then(Value::as_array)
            .map(|os| {
                os.iter()
                    .map(|o| QuestionOption {
                        id: o
                            .get("optionId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        label: o
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        description: o.get("kind").and_then(Value::as_str).map(str::to_string),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        allow_free_text: false,
    }];
    it
}

/// The ACP response for a decision: an explicit option choice wins, else the option whose
/// `kind` matches (allow → allow_once, allow_always → allow_always, deny → reject_once), with
/// fallbacks inside the same polarity; no option at all → `cancelled`.
pub fn decision_json(it: &Interaction, a: &Answer) -> Value {
    let opts: Vec<(String, String)> = it
        .questions
        .iter()
        .find(|q| q.id == "acp_options")
        .map(|q| {
            q.options
                .iter()
                .map(|o| (o.id.clone(), o.description.clone().unwrap_or_default()))
                .collect()
        })
        .unwrap_or_default();
    let explicit = a
        .choices
        .iter()
        .find(|(q, _)| q == "acp_options")
        .and_then(|(_, v)| v.first())
        .filter(|id| opts.iter().any(|(o, _)| o == *id))
        .cloned();
    let by_kind = |kinds: &[&str]| {
        kinds.iter().find_map(|k| {
            opts.iter()
                .find(|(_, kind)| kind == k)
                .map(|(id, _)| id.clone())
        })
    };
    let chosen = explicit.or_else(|| match a.decision {
        Some(Decision::Allow) => by_kind(&["allow_once", "allow_always"]),
        Some(Decision::AllowAlways) => by_kind(&["allow_always", "allow_once"]),
        Some(Decision::Deny) | None => by_kind(&["reject_once", "reject_always"]),
    });
    match chosen {
        Some(id) => json!({"outcome": {"outcome": "selected", "optionId": id}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
}

/// `agent.start --acp "<cmd …>"` (or `--acp` with `--harness <id>`): launch the command through
/// the ACP host. `--harness gemini --acp ""` uses Gemini's manifest `acp_argv`.
pub(super) async fn start(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> R {
    let (h, args) = harness_and_args(p)?;
    let mut q = p.clone();
    if let Some(o) = q.as_object_mut() {
        o.remove("acp");
        o.insert("harness".into(), json!(h.id()));
        o.insert("args".into(), json!(args));
    }
    if method == "agent.spawn" {
        // Reuse the split + start flow with the rewritten params.
        return Box::pin(super::api(server, ctx, "agent.spawn", &q))
            .await
            .unwrap_or_else(|| Err(invalid("agent.spawn unavailable")));
    }
    let pane = resolve_pane(server, ctx, s(&q, "pane"))?;
    start_in_pane(
        server,
        &pane.id,
        h.id(),
        s(&q, "name"),
        s(&q, "prompt"),
        &args,
        None,
    )
    .await
}

/// The ACP agent command of `agent.start --acp … --mode headless`: the harness and the full
/// argv (the manifest's `acp_argv`, then the `--acp` words and `args`).
pub(super) fn resolve(p: &Value) -> Result<(Harness, Vec<String>), vk_proto::rpc::RpcError> {
    let (h, args) = harness_and_args(p)?;
    let mut argv: Vec<String> = h
        .manifest()
        .map(|l| l.m.launch.acp_argv.clone())
        .unwrap_or_default();
    argv.extend(args);
    if argv.is_empty() {
        return Err(invalid("--acp needs a command"));
    }
    Ok((h, argv))
}

fn harness_and_args(p: &Value) -> Result<(Harness, Vec<String>), vk_proto::rpc::RpcError> {
    let acp = s(p, "acp").unwrap_or("").trim();
    let base = s(p, "harness").filter(|h| !h.is_empty() && *h != "acp");
    let mut args: Vec<String> = if acp.is_empty() || acp == "true" {
        vec![]
    } else {
        split_words(acp)
    };
    if let Some(a) = p.get("args").and_then(Value::as_array) {
        args.extend(a.iter().filter_map(|v| v.as_str().map(str::to_string)));
    }
    let id = match base {
        Some(b) => format!("acp:{b}"),
        None => match args.first() {
            Some(cmd) => {
                let name: String = cmd
                    .rsplit('/')
                    .next()
                    .unwrap_or(cmd)
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                            c.to_ascii_lowercase()
                        } else {
                            '-'
                        }
                    })
                    .collect();
                if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_lowercase()) {
                    "acp".to_string()
                } else {
                    format!("acp:{name}")
                }
            }
            None => {
                return Err(invalid(
                    "--acp needs a command, or --harness <id> with an acp_argv",
                ));
            }
        },
    };
    let h = Harness::from_id(&id).ok_or_else(|| invalid(format!("unknown harness {id}")))?;
    if h.manifest().is_some_and(|l| l.m.launch.acp_argv.is_empty()) && args.is_empty() {
        return Err(invalid(format!(
            "{id}: no acp_argv in its manifest; pass --acp \"<cmd>\""
        )));
    }
    Ok((h, args))
}

/// Minimal shell-style word splitting for `--acp "<cmd …>"` (quotes, backslash escapes).
pub fn split_words(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut have = false;
    let mut q: Option<char> = None;
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match (q, c) {
            (None, '\'' | '"') => {
                q = Some(c);
                have = true;
            }
            (Some(qc), c) if c == qc => q = None,
            (None | Some('"'), '\\') => {
                if let Some(n) = it.next() {
                    cur.push(n);
                }
                have = true;
            }
            (None, c) if c.is_whitespace() => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            (_, c) => {
                cur.push(c);
                have = true;
            }
        }
    }
    if have {
        out.push(cur);
    }
    out
}

// ---- host (runs in the pane) -----------------------------------------------------------------

/// Connection to the Vibeke socket with the pane token (same protocol as the hook shim).
struct Link {
    stream: UnixStream,
    rd: BufReader<UnixStream>,
    next: u64,
}

impl Link {
    fn connect() -> Option<Link> {
        if std::env::var("VIBEKE").as_deref() != Ok("1") {
            return None;
        }
        let socket = std::env::var("VIBEKE_SOCKET").ok()?;
        let token = std::env::var("VIBEKE_PANE_TOKEN").ok()?;
        let stream = UnixStream::connect(socket).ok()?;
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let rd = BufReader::new(stream.try_clone().ok()?);
        let mut l = Link {
            stream,
            rd,
            next: 1,
        };
        l.call(
            "client.hello",
            json!({"client": "vibeke-acp-host", "kind": "agent", "token": token, "version": vk_proto::VERSION}),
            Some(Duration::from_secs(5)),
        )?;
        Some(l)
    }

    fn call(&mut self, method: &str, params: Value, timeout: Option<Duration>) -> Option<Value> {
        let id = self.next;
        self.next += 1;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_string(&req).ok()?;
        line.push('\n');
        self.stream.write_all(line.as_bytes()).ok()?;
        let _ = self.rd.get_ref().set_read_timeout(timeout);
        loop {
            let mut resp = String::new();
            if self.rd.read_line(&mut resp).ok()? == 0 {
                return None;
            }
            let v: Value = serde_json::from_str(&resp).ok()?;
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                return v.get("result").cloned();
            }
        }
    }
}

pub enum Msg {
    Agent(Value),
    AgentEof,
    Input(Vec<u8>),
    Gate {
        req: Value,
        decision: Option<Value>,
        interaction: Option<String>,
        key: Option<String>,
    },
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Pending {
    Initialize,
    NewSession,
    Prompt,
}

struct Permission {
    req: Value,
    options: Vec<(String, String, String)>,
    tool_call: String,
}

/// Gate completion: `(decision, interaction id, idempotency key)`.
pub type GateReply = Box<dyn FnOnce(Option<Value>, Option<String>, Option<String>) + Send>;

/// What the host sends to Vibeke; tests substitute a recorder.
pub trait Sink: Send {
    fn signal(&mut self, event: &str, payload: Value);
    /// Open the interaction and wait (on another thread) for a decision.
    fn gate(&mut self, payload: Value, reply: GateReply);
    fn ack(&mut self, interaction: &str, key: &str);
}

struct SocketSink {
    harness: String,
    tx: Option<mpsc::Sender<(String, Value)>>,
}

impl SocketSink {
    fn new(harness: &str) -> SocketSink {
        // One persistent connection for fire-and-forget signals, on its own thread so the
        // protocol loop never blocks on the server.
        let tx = Link::connect().map(|mut link| {
            let (tx, rx) = mpsc::channel::<(String, Value)>();
            std::thread::spawn(move || {
                for (method, params) in rx {
                    let _ = link.call(&method, params, Some(Duration::from_secs(5)));
                }
            });
            tx
        });
        SocketSink {
            harness: harness.to_string(),
            tx,
        }
    }
}

impl Sink for SocketSink {
    fn signal(&mut self, event: &str, payload: Value) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((
                "adapter.signal".into(),
                json!({"harness": self.harness, "event": event, "payload": payload}),
            ));
        }
    }
    fn gate(&mut self, payload: Value, reply: GateReply) {
        let harness = self.harness.clone();
        std::thread::spawn(move || {
            let r = Link::connect().and_then(|mut l| {
                l.call(
                    "adapter.gate",
                    json!({"harness": harness, "event": "RequestPermission", "payload": payload}),
                    Some(Duration::from_secs(1900)),
                )
            });
            let decision = r
                .as_ref()
                .and_then(|v| v.get("decision"))
                .filter(|d| d.is_object())
                .cloned();
            let s = |k: &str| {
                r.as_ref()
                    .and_then(|v| v.get(k))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            reply(decision, s("interaction"), s("idempotency_key"));
        });
    }
    fn ack(&mut self, interaction: &str, key: &str) {
        if let Some(tx) = &self.tx {
            let _ = tx.send((
                "adapter.delivery_ack".into(),
                json!({"interaction": interaction, "idempotency_key": key, "applied": true}),
            ));
        }
    }
}

/// The ACP client state machine, independent of process/terminal plumbing so tests can drive
/// it with an in-process fake agent.
pub struct Host<W: Write> {
    agent: W,
    out: Box<dyn Write + Send>,
    sink: Box<dyn Sink>,
    cwd: std::path::PathBuf,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    session: Option<String>,
    turn_active: bool,
    turn_text: String,
    queued: Vec<String>,
    line: String,
    permission: Option<Permission>,
    /// toolCallId → (title, rawInput, kind) from `tool_call`, for later updates/permissions.
    tools: HashMap<String, (String, Value, String)>,
    gate_tx: mpsc::Sender<Msg>,
    pub exit: bool,
    initial_prompt: Option<String>,
    resume: Option<String>,
    load_session: bool,
}

impl<W: Write> Host<W> {
    pub fn new(
        agent: W,
        out: Box<dyn Write + Send>,
        sink: Box<dyn Sink>,
        cwd: std::path::PathBuf,
        gate_tx: mpsc::Sender<Msg>,
        initial_prompt: Option<String>,
        resume: Option<String>,
    ) -> Self {
        Host {
            agent,
            out,
            sink,
            cwd,
            next_id: 1,
            pending: HashMap::new(),
            session: None,
            turn_active: false,
            turn_text: String::new(),
            queued: vec![],
            line: String::new(),
            permission: None,
            tools: HashMap::new(),
            gate_tx,
            exit: false,
            initial_prompt,
            resume,
            load_session: false,
        }
    }

    fn print(&mut self, s: &str) {
        let _ = self.out.write_all(s.replace('\n', "\r\n").as_bytes());
        let _ = self.out.flush();
    }

    fn send(&mut self, v: Value) {
        let mut line = serde_json::to_string(&v).unwrap_or_default();
        line.push('\n');
        let _ = self.agent.write_all(line.as_bytes());
        let _ = self.agent.flush();
    }

    fn request(&mut self, method: &str, params: Value, kind: Pending) {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, kind);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    }

    fn respond(&mut self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn respond_err(&mut self, id: &Value, code: i64, message: &str) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
    }

    pub fn start(&mut self) {
        self.request(
            "initialize",
            json!({"protocolVersion": PROTOCOL_VERSION, "clientCapabilities": {"fs": {"readTextFile": true, "writeTextFile": true}, "terminal": false}}),
            Pending::Initialize,
        );
    }

    fn prompt_line(&mut self) {
        let l = self.line.clone();
        self.print(&format!("\nacp› {l}"));
    }

    fn submit(&mut self, text: String) {
        if text.trim().is_empty() {
            self.prompt_line();
            return;
        }
        let Some(session) = self.session.clone() else {
            self.queued.push(text);
            return;
        };
        if self.turn_active {
            self.print("\n(queued until the current turn ends)");
            self.queued.push(text);
            return;
        }
        self.turn_active = true;
        self.turn_text.clear();
        self.sink.signal(
            "UserPromptSubmit",
            json!({"prompt": text, "session_id": session}),
        );
        self.print("\n… working (esc to interrupt)\n");
        self.request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
            Pending::Prompt,
        );
    }

    fn cancel(&mut self) {
        if let Some(s) = self.session.clone()
            && self.turn_active
        {
            self.send(
                json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": s}}),
            );
            self.print("\n(cancel requested)");
        }
    }

    /// Answer the open permission request with `option_id` (from the pane or from Vibeke).
    fn answer_permission(&mut self, option_id: Option<String>, from_vibeke: bool) -> bool {
        let Some(p) = self.permission.take() else {
            return false;
        };
        let id = p.req.get("id").cloned().unwrap_or(Value::Null);
        let outcome = match &option_id {
            Some(o) => json!({"outcome": {"outcome": "selected", "optionId": o}}),
            None => json!({"outcome": {"outcome": "cancelled"}}),
        };
        self.respond(&id, outcome);
        let name = option_id
            .as_ref()
            .and_then(|o| p.options.iter().find(|(id, _, _)| id == o))
            .map(|(_, n, _)| n.clone())
            .unwrap_or_else(|| "cancelled".into());
        self.print(&format!(
            "\n✓ {name}{}\n",
            if from_vibeke {
                " (answered from Vibeke)"
            } else {
                ""
            }
        ));
        if !from_vibeke {
            self.sink.signal(
                "PermissionResolved",
                json!({"tool_use_id": p.tool_call, "option_id": option_id}),
            );
        }
        true
    }

    fn render_permission(&mut self, title: &str, command: Option<&str>) {
        let mut s = format!("\n⏵ permission: {title}\n");
        if let Some(c) = command {
            s.push_str(&format!("  $ {c}\n"));
        }
        if let Some(p) = &self.permission {
            for (i, (_, name, _)) in p.options.iter().enumerate() {
                s.push_str(&format!("  {}. {name}\n", i + 1));
            }
            let n = p.options.len();
            s.push_str(&format!("Choose 1-{n} then Enter: "));
        }
        self.line.clear();
        self.print(&s);
    }

    pub fn on_input(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes)
            .replace("\x1b[200~", "")
            .replace("\x1b[201~", "");
        let b = text.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            match c {
                b'\r' | b'\n' => {
                    let line = std::mem::take(&mut self.line);
                    if self.permission.is_some() {
                        let n: usize = line.trim().parse().unwrap_or(0);
                        let opt = self
                            .permission
                            .as_ref()
                            .and_then(|p| p.options.get(n.wrapping_sub(1)))
                            .map(|o| o.0.clone());
                        match opt {
                            Some(o) => {
                                self.answer_permission(Some(o), false);
                            }
                            None => {
                                let n = self
                                    .permission
                                    .as_ref()
                                    .map(|p| p.options.len())
                                    .unwrap_or(0);
                                self.print(&format!("\nChoose 1-{n} then Enter: "));
                            }
                        }
                    } else {
                        self.submit(line);
                    }
                    // A lone \n after \r is part of the same Enter.
                    if c == b'\r' && b.get(i + 1) == Some(&b'\n') {
                        i += 1;
                    }
                }
                0x1b => {
                    if b.get(i + 1) == Some(&b'[') || b.get(i + 1) == Some(&b'O') {
                        // CSI/SS3 sequence (arrows etc.): skip to its final byte.
                        i += 2;
                        while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                            i += 1;
                        }
                    } else if self.permission.is_some() {
                        self.answer_permission(None, false);
                    } else {
                        self.cancel();
                    }
                }
                0x03 => {
                    if self.turn_active {
                        self.cancel();
                    } else if !self.line.is_empty() {
                        self.line.clear();
                        self.prompt_line();
                    } else {
                        self.exit = true;
                    }
                }
                0x04 if self.line.is_empty() && !self.turn_active => self.exit = true,
                0x7f | 0x08 => {
                    if self.line.pop().is_some() {
                        self.print("\x08 \x08");
                    }
                }
                c if c >= 0x20 => {
                    // Keep UTF-8 sequences intact.
                    let start = i;
                    let mut end = i + 1;
                    while end < b.len() && (b[end] & 0xc0) == 0x80 {
                        end += 1;
                    }
                    let s = String::from_utf8_lossy(&b[start..end]).to_string();
                    self.line.push_str(&s);
                    self.print(&s);
                    i = end - 1;
                }
                _ => {}
            }
            i += 1;
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
        // Canonicalize the existing prefix (the file itself may not exist yet).
        let parent = p.parent()?.canonicalize().ok()?;
        let full = parent.join(p.file_name()?);
        full.starts_with(&root).then_some(full)
    }

    pub fn on_agent(&mut self, v: Value) {
        let method = v.get("method").and_then(Value::as_str).map(str::to_string);
        let id = v.get("id").cloned();
        match (method.as_deref(), id) {
            // Response to one of our requests.
            (None, Some(id)) => {
                let Some(kind) = id.as_u64().and_then(|i| self.pending.remove(&i)) else {
                    return;
                };
                let err = v.get("error").cloned();
                let result = v.get("result").cloned().unwrap_or(Value::Null);
                self.on_response(kind, result, err);
            }
            (Some("session/update"), None) => self.on_update(&v["params"]),
            (Some("session/request_permission"), Some(_)) => {
                let params = v.get("params").cloned().unwrap_or(Value::Null);
                let tc = params.get("toolCall").cloned().unwrap_or(Value::Null);
                let options: Vec<(String, String, String)> = params
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|os| {
                        os.iter()
                            .map(|o| {
                                let g = |k: &str| {
                                    o.get(k).and_then(Value::as_str).unwrap_or("").to_string()
                                };
                                (g("optionId"), g("name"), g("kind"))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let tool_call = tc
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // A permission can refer to a tool call announced earlier with more detail.
                let known = self.tools.get(&tool_call).cloned();
                let mut tc_full = tc.clone();
                if let (Some((title, raw, _)), Some(o)) = (known, tc_full.as_object_mut()) {
                    o.entry("title").or_insert(json!(title));
                    o.entry("rawInput").or_insert(raw);
                }
                let title = tc_full
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("tool call")
                    .to_string();
                let command = tc_full.get("rawInput").and_then(command_of);
                if self.permission.is_some() {
                    // One prompt at a time: refuse a concurrent request rather than mixing answers.
                    let id = v["id"].clone();
                    self.respond(&id, json!({"outcome": {"outcome": "cancelled"}}));
                    return;
                }
                self.permission = Some(Permission {
                    req: v.clone(),
                    options,
                    tool_call,
                });
                self.render_permission(&title, command.as_deref());
                let tx = self.gate_tx.clone();
                let req = v.clone();
                let mut payload = params.clone();
                payload["toolCall"] = tc_full;
                self.sink.gate(
                    payload,
                    Box::new(move |decision, interaction, key| {
                        let _ = tx.send(Msg::Gate {
                            req,
                            decision,
                            interaction,
                            key,
                        });
                    }),
                );
            }
            (Some("fs/read_text_file"), Some(id)) => {
                let path = v
                    .pointer("/params/path")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match self
                    .within_cwd(path)
                    .and_then(|p| std::fs::read_to_string(p).ok())
                {
                    Some(text) => {
                        let line = v.pointer("/params/line").and_then(Value::as_u64);
                        let limit = v.pointer("/params/limit").and_then(Value::as_u64);
                        let content = match (line, limit) {
                            (None, None) => text,
                            _ => text
                                .lines()
                                .skip(line.unwrap_or(1).saturating_sub(1) as usize)
                                .take(limit.unwrap_or(u64::MAX) as usize)
                                .collect::<Vec<_>>()
                                .join("\n"),
                        };
                        self.respond(&id, json!({"content": content}));
                    }
                    None => self.respond_err(
                        &id,
                        -32002,
                        "file not readable or outside the session cwd",
                    ),
                }
            }
            (Some("fs/write_text_file"), Some(id)) => {
                let path = v
                    .pointer("/params/path")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let content = v
                    .pointer("/params/content")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match self.within_cwd(path) {
                    Some(p) if std::fs::write(&p, content).is_ok() => {
                        self.respond(&id, Value::Null)
                    }
                    _ => self.respond_err(&id, -32002, "write refused: outside the session cwd"),
                }
            }
            (Some(_), Some(id)) => self.respond_err(&id, -32601, "method not supported by vibeke"),
            _ => {}
        }
    }

    fn on_response(&mut self, kind: Pending, result: Value, err: Option<Value>) {
        if let Some(e) = err {
            let msg = e
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string();
            self.print(&format!("\n! {msg}\n"));
            match kind {
                Pending::Prompt => {
                    self.turn_active = false;
                    self.sink.signal("StopFailure", json!({"error_type": msg}));
                    self.prompt_line();
                }
                _ => self.exit = true,
            }
            return;
        }
        match kind {
            Pending::Initialize => {
                self.load_session = result
                    .pointer("/agentCapabilities/loadSession")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let cwd = self.cwd.to_string_lossy().to_string();
                match (self.resume.clone(), self.load_session) {
                    (Some(sid), true) => {
                        self.session = Some(sid.clone());
                        self.request(
                            "session/load",
                            json!({"sessionId": sid, "cwd": cwd, "mcpServers": []}),
                            Pending::NewSession,
                        );
                    }
                    _ => self.request(
                        "session/new",
                        json!({"cwd": cwd, "mcpServers": []}),
                        Pending::NewSession,
                    ),
                }
            }
            Pending::NewSession => {
                if let Some(s) = result.get("sessionId").and_then(Value::as_str) {
                    self.session = Some(s.to_string());
                }
                let Some(sid) = self.session.clone() else {
                    self.print("\n! agent returned no sessionId\n");
                    self.exit = true;
                    return;
                };
                self.sink.signal(
                    "SessionStart",
                    json!({"session_id": sid, "cwd": self.cwd.to_string_lossy(), "source": if self.resume.is_some() { "resume" } else { "startup" }}),
                );
                self.print(&format!("session {sid}\n"));
                let mut q = std::mem::take(&mut self.queued);
                if let Some(p) = self.initial_prompt.take() {
                    q.insert(0, p);
                }
                if q.is_empty() {
                    self.prompt_line();
                } else {
                    let first = q.remove(0);
                    self.queued = q;
                    self.submit(first);
                }
            }
            Pending::Prompt => {
                self.turn_active = false;
                let stop = result
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn")
                    .to_string();
                if let Some(u) = result
                    .get("usage")
                    .or_else(|| result.pointer("/_meta/usage"))
                {
                    self.sink.signal("Usage", u.clone());
                }
                let msg = self.turn_text.trim().to_string();
                if stop == "refusal" || stop == "max_tokens" {
                    self.sink.signal("StopFailure", json!({"error_type": stop}));
                } else {
                    self.sink.signal(
                        "Stop",
                        json!({"session_id": self.session, "last_assistant_message": msg, "stop_reason": stop}),
                    );
                }
                self.print(&format!("\n[{stop}]"));
                if self.queued.is_empty() {
                    self.prompt_line();
                } else {
                    let next = self.queued.remove(0);
                    self.submit(next);
                }
            }
        }
    }

    fn on_update(&mut self, params: &Value) {
        let u = &params["update"];
        match u.get("sessionUpdate").and_then(Value::as_str).unwrap_or("") {
            "agent_message_chunk" => {
                if let Some(t) = u.pointer("/content/text").and_then(Value::as_str) {
                    self.turn_text.push_str(t);
                    self.print(t);
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
                let kind = u.get("kind").and_then(Value::as_str).unwrap_or("other");
                let raw = u.get("rawInput").cloned().unwrap_or(json!({}));
                let tool = tool_name(kind, &title);
                let mut input = raw.clone();
                if let (Some(o), Some(path)) = (
                    input.as_object_mut(),
                    u.pointer("/locations/0/path").and_then(Value::as_str),
                ) {
                    o.entry("file_path").or_insert(json!(path));
                }
                self.tools
                    .insert(id.clone(), (title.clone(), raw, kind.to_string()));
                self.print(&format!("\n⏺ {title}\n"));
                self.sink.signal(
                    "PreToolUse",
                    json!({"tool_name": tool, "tool_input": input, "tool_use_id": id}),
                );
                if matches!(
                    u.get("status").and_then(Value::as_str),
                    Some("completed" | "failed")
                ) {
                    self.tool_done(u);
                }
            }
            "tool_call_update" => {
                if matches!(
                    u.get("status").and_then(Value::as_str),
                    Some("completed" | "failed")
                ) {
                    self.tool_done(u);
                }
            }
            "plan" => {
                let entries = u
                    .get("entries")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut s = String::from("\nplan:\n");
                for e in entries {
                    let st = e.get("status").and_then(Value::as_str).unwrap_or("pending");
                    let mark = match st {
                        "completed" => "✓",
                        "in_progress" => "▸",
                        _ => "·",
                    };
                    s.push_str(&format!(
                        "  {mark} {}\n",
                        e.get("content").and_then(Value::as_str).unwrap_or("")
                    ));
                }
                self.print(&s);
            }
            _ => {}
        }
    }

    fn tool_done(&mut self, u: &Value) {
        let id = u
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let failed = u.get("status").and_then(Value::as_str) == Some("failed");
        let (title, raw, known_kind) = self
            .tools
            .remove(&id)
            .unwrap_or_else(|| ("tool".into(), json!({}), "other".into()));
        let kind = u
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or(known_kind.as_str())
            .to_string();
        let kind = kind.as_str();
        let mut input = raw;
        if let (Some(o), Some(path)) = (
            input.as_object_mut(),
            u.pointer("/locations/0/path").and_then(Value::as_str),
        ) {
            o.entry("file_path").or_insert(json!(path));
        }
        let exit = u
            .pointer("/rawOutput/exit_code")
            .or_else(|| u.pointer("/rawOutput/exitCode"))
            .cloned();
        self.print(&format!("  {} {title}\n", if failed { "✗" } else { "✓" }));
        self.sink.signal(
            if failed { "PostToolUseFailure" } else { "PostToolUse" },
            json!({"tool_name": tool_name(kind, &title), "tool_use_id": id, "tool_input": input, "exit_code": exit}),
        );
    }

    pub fn on_gate(
        &mut self,
        req: Value,
        decision: Option<Value>,
        interaction: Option<String>,
        key: Option<String>,
    ) {
        let same = self
            .permission
            .as_ref()
            .is_some_and(|p| p.req.get("id") == req.get("id"));
        let Some(d) = decision.filter(|_| same) else {
            // No decision (observe mode, release-on-focus, timeout): the prompt in the pane stays.
            return;
        };
        let opt = d
            .pointer("/outcome/optionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        if self.answer_permission(opt, true)
            && let (Some(i), Some(k)) = (interaction, key)
        {
            self.sink.ack(&i, &k);
        }
    }

    pub fn handle(&mut self, m: Msg) {
        match m {
            Msg::Agent(v) => self.on_agent(v),
            Msg::AgentEof => {
                self.sink
                    .signal("SessionEnd", json!({"reason": "agent_exited"}));
                self.exit = true;
            }
            Msg::Input(b) => self.on_input(&b),
            Msg::Gate {
                req,
                decision,
                interaction,
                key,
            } => self.on_gate(req, decision, interaction, key),
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

struct RawMode(Option<libc::termios>);

impl RawMode {
    fn enable() -> RawMode {
        // SAFETY: tcgetattr/tcsetattr on stdin with a zeroed termios out-param.
        unsafe {
            if libc::isatty(0) != 1 {
                return RawMode(None);
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return RawMode(None);
            }
            let orig = t;
            t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(0, libc::TCSANOW, &t);
            RawMode(Some(orig))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            // SAFETY: restoring the attributes read in `enable`.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, t);
            }
        }
    }
}

fn spawn_agent(argv: &[String], cwd: &std::path::Path) -> std::io::Result<Child> {
    Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

/// `vibeke acp-host [--harness <id>] [--prompt <text>] [--resume <session>] -- <agent argv…>`
pub fn host_main(args: &[String]) -> i32 {
    let mut harness = "acp".to_string();
    let mut prompt = None;
    let mut resume = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--harness" => {
                harness = args.get(i + 1).cloned().unwrap_or(harness);
                i += 2;
            }
            "--prompt" => {
                prompt = args.get(i + 1).cloned();
                i += 2;
            }
            "--resume" => {
                resume = args.get(i + 1).cloned();
                i += 2;
            }
            "--" => {
                i += 1;
                break;
            }
            _ => break,
        }
    }
    let argv: Vec<String> = args[i..].to_vec();
    if argv.is_empty() {
        eprintln!("usage: vibeke acp-host [--harness id] [--prompt text] -- <agent command…>");
        return 2;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut child = match spawn_agent(&argv, &cwd) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("acp-host: cannot start {}: {e}", argv[0]);
            return 127;
        }
    };
    let stdin: ChildStdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<Msg>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    let _ = tx.send(Msg::Agent(v));
                }
            }
            let _ = tx.send(Msg::AgentEof);
        });
    }
    // Agent diagnostics: kept off the pane, last lines shown on exit.
    let tail = Arc::new(Mutex::new(std::collections::VecDeque::<String>::new()));
    {
        let tail = tail.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut t = tail.lock().unwrap();
                t.push_back(line);
                if t.len() > 20 {
                    t.pop_front();
                }
            }
        });
    }
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let mut inp = std::io::stdin();
            loop {
                match inp.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(Msg::Input(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    let raw = RawMode::enable();
    let name = harness.clone();
    let mut host = Host::new(
        stdin,
        Box::new(std::io::stdout()),
        Box::new(SocketSink::new(&harness)),
        cwd,
        tx,
        prompt,
        resume,
    );
    host.print(&format!("vibeke acp-host · {name} · {}\n", argv.join(" ")));
    host.start();
    while !host.exit {
        match rx.recv() {
            Ok(m) => host.handle(m),
            Err(_) => break,
        }
    }
    drop(host);
    let _ = child.kill();
    let status = child.wait().ok().and_then(|s| s.code()).unwrap_or(0);
    drop(raw);
    let t = tail.lock().unwrap();
    if !t.is_empty() && status != 0 {
        eprintln!("\nagent stderr (last lines):");
        for l in t.iter() {
            eprintln!("  {l}");
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    type Log = Arc<Mutex<Vec<(String, Value)>>>;

    struct Rec {
        log: Log,
        decide: Option<Value>,
    }

    impl Sink for Rec {
        fn signal(&mut self, event: &str, payload: Value) {
            self.log.lock().unwrap().push((event.into(), payload));
        }
        fn gate(&mut self, payload: Value, reply: GateReply) {
            self.log.lock().unwrap().push(("gate".into(), payload));
            reply(self.decide.clone(), Some("i1".into()), Some("i1:1".into()));
        }
        fn ack(&mut self, interaction: &str, key: &str) {
            self.log
                .lock()
                .unwrap()
                .push(("ack".into(), json!([interaction, key])));
        }
    }

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn sent(b: &Buf) -> Vec<Value> {
        String::from_utf8_lossy(&b.0.lock().unwrap())
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn host(decide: Option<Value>) -> (Host<Buf>, Buf, Buf, Log, mpsc::Receiver<Msg>) {
        let agent = Buf::default();
        let screen = Buf::default();
        let log: Log = Arc::default();
        let (tx, rx) = mpsc::channel();
        let h = Host::new(
            agent.clone(),
            Box::new(screen.clone()),
            Box::new(Rec {
                log: log.clone(),
                decide,
            }),
            std::env::temp_dir(),
            tx,
            None,
            None,
        );
        (h, agent, screen, log, rx)
    }

    fn events(log: &Log) -> Vec<String> {
        log.lock().unwrap().iter().map(|(e, _)| e.clone()).collect()
    }

    #[test]
    fn full_turn_with_a_vibeke_answered_permission() {
        let allow = json!({"outcome": {"outcome": "selected", "optionId": "ok-once"}});
        let (mut h, agent, screen, log, rx) = host(Some(allow));
        h.start();
        assert_eq!(sent(&agent)[0]["method"], "initialize");
        h.on_agent(json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": 1, "agentCapabilities": {}}}));
        assert_eq!(sent(&agent)[1]["method"], "session/new");
        h.on_agent(json!({"jsonrpc": "2.0", "id": 2, "result": {"sessionId": "sess-a"}}));
        assert_eq!(events(&log), vec!["SessionStart"]);
        h.on_input(b"run the tests\r");
        let p = &sent(&agent)[2];
        assert_eq!(p["method"], "session/prompt");
        assert_eq!(p["params"]["prompt"][0]["text"], "run the tests");
        h.on_agent(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "sess-a", "update": {"sessionUpdate": "tool_call", "toolCallId": "tc1", "title": "cargo test", "kind": "execute", "status": "pending", "rawInput": {"command": "cargo test"}}}}));
        h.on_agent(json!({"jsonrpc": "2.0", "id": 77, "method": "session/request_permission", "params": {"sessionId": "sess-a", "toolCall": {"toolCallId": "tc1"}, "options": [
            {"optionId": "ok-once", "name": "Allow once", "kind": "allow_once"},
            {"optionId": "ok-always", "name": "Allow always", "kind": "allow_always"},
            {"optionId": "no", "name": "Reject", "kind": "reject_once"}]}}));
        // The gate payload was enriched with the earlier tool_call's title/rawInput.
        let gate = log
            .lock()
            .unwrap()
            .iter()
            .find(|(e, _)| e == "gate")
            .unwrap()
            .1
            .clone();
        assert_eq!(gate["toolCall"]["rawInput"]["command"], "cargo test");
        let shown = String::from_utf8_lossy(&screen.0.lock().unwrap()).to_string();
        assert!(shown.contains("⏵ permission: cargo test"));
        assert!(shown.contains("  $ cargo test"));
        assert!(shown.contains("  3. Reject"));
        // The gate thread's reply arrives as a message.
        let m = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        h.handle(m);
        let r = sent(&agent).into_iter().find(|v| v["id"] == 77).unwrap();
        assert_eq!(r["result"]["outcome"]["optionId"], "ok-once");
        assert!(events(&log).contains(&"ack".to_string()));
        h.on_agent(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "sess-a", "update": {"sessionUpdate": "tool_call_update", "toolCallId": "tc1", "status": "completed", "rawOutput": {"exit_code": 0}}}}));
        h.on_agent(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "sess-a", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "All tests pass."}}}}));
        h.on_agent(json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "end_turn"}}));
        assert_eq!(
            events(&log),
            vec![
                "SessionStart",
                "UserPromptSubmit",
                "PreToolUse",
                "gate",
                "ack",
                "PostToolUse",
                "Stop"
            ]
        );
        let l = log.lock().unwrap();
        let stop = &l.iter().find(|(e, _)| e == "Stop").unwrap().1;
        assert_eq!(stop["last_assistant_message"], "All tests pass.");
        let post = &l.iter().find(|(e, _)| e == "PostToolUse").unwrap().1;
        assert_eq!(post["tool_name"], "Bash");
        assert_eq!(post["exit_code"], 0);
    }

    #[test]
    fn pane_answer_resolves_and_cancel_and_fs_scope() {
        let (mut h, agent, _screen, log, _rx) = host(None);
        h.start();
        h.on_agent(json!({"id": 1, "result": {}}));
        h.on_agent(json!({"id": 2, "result": {"sessionId": "s"}}));
        h.on_input(b"go\r");
        h.on_agent(json!({"id": 9, "method": "session/request_permission", "params": {"toolCall": {"toolCallId": "t", "title": "rm -rf x", "rawInput": {"command": ["rm", "-rf", "x"]}}, "options": [
            {"optionId": "a", "name": "Allow once", "kind": "allow_once"},
            {"optionId": "r", "name": "Reject", "kind": "reject_once"}]}}));
        // Gate said nothing (observe mode): the user answers in the pane.
        h.on_input(b"9\r"); // out of range: re-asked
        h.on_input(b"2\r");
        let r = sent(&agent).into_iter().find(|v| v["id"] == 9).unwrap();
        assert_eq!(r["result"]["outcome"]["optionId"], "r");
        assert!(events(&log).contains(&"PermissionResolved".to_string()));
        h.on_input(b"\x1b");
        assert!(sent(&agent).iter().any(|v| v["method"] == "session/cancel"));
        h.on_agent(
            json!({"id": 50, "method": "fs/read_text_file", "params": {"path": "/etc/passwd"}}),
        );
        let r = sent(&agent).into_iter().find(|v| v["id"] == 50).unwrap();
        assert!(
            r.get("error").is_some(),
            "reads outside the cwd are refused"
        );
        h.on_agent(json!({"id": 51, "method": "terminal/create", "params": {}}));
        let r = sent(&agent).into_iter().find(|v| v["id"] == 51).unwrap();
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn decisions_map_to_option_ids() {
        let it = permission_interaction(
            &json!({"toolCall": {"toolCallId": "t1", "title": "Edit a.rs", "kind": "edit", "locations": [{"path": "/w/a.rs"}]},
            "options": [{"optionId": "o1", "name": "Allow", "kind": "allow_once"}, {"optionId": "o2", "name": "Always", "kind": "allow_always"}, {"optionId": "o3", "name": "No", "kind": "reject_once"}]}),
        );
        assert_eq!(it.native_ref.as_deref(), Some("t1"));
        assert_eq!(it.action.as_ref().unwrap().paths, vec!["/w/a.rs"]);
        let pick = |d: Option<Decision>, choices: Vec<(String, Vec<String>)>| {
            decision_json(
                &it,
                &Answer {
                    decision: d,
                    choices,
                    text: None,
                },
            )["outcome"]["optionId"]
                .clone()
        };
        assert_eq!(pick(Some(Decision::Allow), vec![]), "o1");
        assert_eq!(pick(Some(Decision::AllowAlways), vec![]), "o2");
        assert_eq!(pick(Some(Decision::Deny), vec![]), "o3");
        assert_eq!(
            pick(None, vec![("acp_options".into(), vec!["o2".into()])]),
            "o2"
        );
        assert_eq!(
            split_words(r#"python3 "my agent.py" --x 'a b' c\ d"#),
            vec!["python3", "my agent.py", "--x", "a b", "c d"]
        );
    }
}
