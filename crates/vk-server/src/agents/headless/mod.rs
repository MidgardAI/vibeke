//! Headless harness runs (01 §1.2, §3.3; 04 §3.3, §6.1.3, §6.2, §6.3, §6.6): the harness speaks
//! its stdio protocol under a **pipe-mode holder** and Vibeke's in-server adapter is the client.
//!
//! | Harness | Launch | Adapter |
//! |---|---|---|
//! | Claude Code | `claude -p --input-format stream-json --output-format stream-json --verbose --permission-prompt-tool stdio` | [`claude`] (stream-json + control requests) |
//! | Codex | `codex app-server` | [`codex`] (JSON-RPC: `thread/*`, `turn/*`, server requests) |
//! | pi / omp | `pi --mode rpc` / `omp --mode rpc` | [`pi`] (RPC commands, events, `extension_ui_request`) |
//! | ACP agents | the agent argv | [`acp`] (`initialize`, `session/new|load`, `session/prompt`) |
//!
//! **Durability.** The holder journals the child's stdout and stderr *and every byte written to
//! its stdin* (holder/2 `Stream::Stdin`), so the journal is the whole conversation in order.
//! Per pane the server keeps a [`Record`] in the store's kv (`headless/<pane>`): harness,
//! protocol, run, session and `processed`, the journal offset up to which every frame was
//! handled with its events emitted. A restarted server replays the journal from the ring start
//! through a fresh adapter: frames at or before `processed` only rebuild state and the
//! transcript; frames after it (the harness kept working while no server was attached) emit
//! their events now, exactly once. Writes are never replayed: requests the journal shows as
//! unanswered are reconciled after the replay (`Adapter::reconcile`) — protocol queries such as
//! Codex `thread/read`, pi `get_state` or ACP `session/load` when the ring overflowed, and
//! re-opening (or, if a decision was recorded but not written, delivering) pending approvals.
//!
//! **Interactions.** A server→client request becomes an Interaction with `native_ref` =
//! `rpc:<request id>`, answered natively over the protocol from any client
//! (`interaction.answer`), by policy, or by typing its number in the pane. Delivery is the
//! recoverable transaction of 04 §7.3: the response is an `Input` whose holder ack marks the
//! interaction `delivered`; after a crash the journal decides (response present → delivered,
//! absent → delivered now, once).
//!
//! **Inputs.** Prompts typed in the pane go through a line editor here, never raw to the
//! child's stdin. Every write is an acked holder input; one still unacked when the server died
//! and absent from the journal is reported as `input_unconfirmed` (01 §1.2), never resent.

pub mod acp;
pub mod acp_term;
pub mod claude;
pub mod codex;
pub mod codex_mux;
pub mod pi;
pub mod transcript;

pub use transcript::{ToolStatus, View};

use super::*;
use crate::pane::PIPE_ARGV0;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use vk_proto::holder::{InputStatus, Stream};

/// Store kv scope of the per-pane [`Record`].
pub const KV_SCOPE: &str = "headless";

/// Wire protocol of a headless run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    StreamJson,
    AppServer,
    Rpc,
    Acp,
}

impl Kind {
    pub fn for_harness(h: Harness) -> Option<Kind> {
        match h.family() {
            harness::Family::Claude => Some(Kind::StreamJson),
            harness::Family::Codex => Some(Kind::AppServer),
            harness::Family::Pi | harness::Family::Omp => Some(Kind::Rpc),
            harness::Family::Acp => Some(Kind::Acp),
            _ => None,
        }
    }

    /// `AgentRun.integration` of a headless run.
    pub fn integration(&self) -> &'static str {
        match self {
            Kind::StreamJson => "headless:stream-json",
            Kind::AppServer => "headless:app-server",
            Kind::Rpc => "headless:rpc",
            Kind::Acp => "headless:acp",
        }
    }

    fn capabilities(&self, h: Harness) -> Vec<String> {
        let mut v = vec![
            "observe",
            "gate",
            "answer_native:approval",
            "answer_native:question",
            "reconcile",
            "resume",
            "survive_disconnect",
            "headless",
        ];
        if *self == Kind::Rpc {
            v[2] = "answer_native:extension_dialog";
            // omp's rpc-ui mode carries its tool approvals as dialogs (04 §6.3).
            if h.base() == Harness::Omp {
                v.push("answer_native:approval");
            }
        }
        v.into_iter().map(str::to_string).collect()
    }

    fn adapter(&self, rec: &Record) -> Box<dyn Adapter> {
        match self {
            Kind::StreamJson => Box::new(claude::Claude::new(rec)),
            Kind::AppServer => Box::new(codex::Codex::new(rec)),
            Kind::Rpc => Box::new(pi::Pi::new(rec)),
            Kind::Acp => Box::new(acp::Acp::new(rec)),
        }
    }
}

pub fn is_headless(run: &AgentRun) -> bool {
    run.integration.starts_with("headless:")
}

/// Per-pane state persisted in kv `headless/<pane>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub harness: String,
    pub kind: Kind,
    pub run: String,
    pub cwd: String,
    /// Session to resume or load (pre-assigned, resumed, or reported by the harness).
    pub session: Option<String>,
    /// The process was started to continue `session` (resume / `session/load`).
    #[serde(default)]
    pub resume: bool,
    /// Journal offset up to which every frame was handled and its events emitted.
    #[serde(default)]
    pub processed: u64,
    /// Inputs written to the holder and not yet acknowledged: (input id, preview).
    #[serde(default)]
    pub unacked: Vec<(u64, String)>,
    /// The ACP agent command (an ad-hoc `--acp "<cmd>"` has no manifest to recover it from).
    #[serde(default)]
    pub acp_argv: Vec<String>,
    /// Prompts acknowledged to the caller but not yet handed to the harness: submitted before
    /// the session was ready, or follow-ups waiting for the running turn (adapters without a
    /// native follow-up queue). Persisted before the ack, so a restart delivers them.
    #[serde(default)]
    pub queued: Vec<Queued>,
    /// Automatic requests (ACP `fs/write_text_file`) whose side effect was carried out: a
    /// replayed or reconciled request answers without repeating it.
    #[serde(default)]
    pub auto_done: Vec<String>,
    /// Vibeke isolates the run (13 §3): host-side services for the agent (ACP `fs/*` and
    /// `terminal/*`, which the server would carry out on the host) are refused.
    #[serde(default)]
    pub isolated: bool,
    /// ACP terminals created for the agent (`terminal/create`), so a restarted server finds
    /// their panes again.
    #[serde(default)]
    pub terminals: Vec<acp_term::Saved>,
}

/// A prompt waiting in [`Record::queued`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Queued {
    pub text: String,
    pub mode: PromptMode,
}

/// At most this many [`Record::auto_done`] ids are kept (the oldest are answered long ago).
const AUTO_DONE_MAX: usize = 256;

impl Record {
    pub fn load(server: &Server, pane: &str) -> Option<Record> {
        server
            .with_core(|c| c.store.kv_get(KV_SCOPE, pane).ok().flatten())
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    /// The session was established before this adapter was (re)built: the harness reported
    /// it and frames were processed. A rebuilt adapter must not redo the handshake even when
    /// the journal no longer shows it.
    pub fn established(&self) -> bool {
        self.processed > 0 && self.session.is_some()
    }

    fn persist(&self, server: &Server, pane: &str) {
        let _ = self.try_persist(server, pane);
    }

    /// Persist, reporting a failure (what must be durable before a side effect).
    fn try_persist(&self, server: &Server, pane: &str) -> Result<(), String> {
        let v = serde_json::to_string(self).map_err(|e| e.to_string())?;
        #[cfg(test)]
        if FAIL_PERSIST.with(|f| f.get()) {
            return Err("injected persistence failure".into());
        }
        server.with_core(|c| {
            let mut tx = Tx::new();
            tx.m.kv(KV_SCOPE, pane, Some(v));
            c.commit(tx).map(|_| ()).map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
thread_local! {
    /// Tests: make [`Record::try_persist`] fail on this thread.
    pub(crate) static FAIL_PERSIST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptMode {
    Send,
    Steer,
    FollowUp,
}

impl PromptMode {
    pub fn parse(s: Option<&str>) -> PromptMode {
        match s {
            Some("steer") => PromptMode::Steer,
            Some("follow_up") | Some("follow-up") => PromptMode::FollowUp,
            _ => PromptMode::Send,
        }
    }
}

/// Commands to a pipe pane's adapter (sent through `PaneCmd::Headless`).
pub enum Cmd {
    /// Bind a freshly spawned pane to its record and run the protocol handshake.
    Attach(Box<Record>),
    Prompt {
        text: String,
        mode: PromptMode,
        ack: Option<oneshot::Sender<Result<(), String>>>,
    },
    Interrupt,
    /// Deliver a recorded decision natively (from `interaction.answer` or policy).
    Answer {
        interaction: String,
        native_ref: String,
        key: String,
    },
    /// An ACP terminal's process exited (its watcher task).
    TerminalExited(String),
}

/// What the pane loop must do for the session: write protocol bytes as an acked holder input,
/// or show transcript text in the pane.
pub enum Act {
    Write { id: u64, bytes: Vec<u8> },
    Render(String),
}

/// A server→client request still waiting for Vibeke.
pub struct Pending {
    pub native_ref: String,
    /// `None`: answered automatically by the adapter (e.g. ACP `fs/*`).
    pub interaction: Option<Interaction>,
}

/// Output of one adapter step, filtered by the [`Session`] (replay suppresses writes; events
/// only for frames past `processed`).
#[derive(Default)]
pub struct Cx {
    writes: Vec<(Value, Option<String>)>,
    /// Transcript output, in order (text and tool calls).
    view: Vec<View>,
    signals: Vec<(String, Value)>,
    opens: Vec<Interaction>,
    resolved: Vec<String>,
    usage: Vec<(RunUsage, bool)>,
    rate_limits: Vec<RateLimitInfo>,
    /// ACP `terminal/*` requests for the session to carry out: (native ref, request).
    terminal: Vec<(String, acp_term::Req)>,
    session: Option<String>,
    /// Set by the session for live steps (never during a journal replay): adapters carry out
    /// side effects of automatic requests only then.
    pub live: bool,
    /// Automatic requests whose side effect was just carried out (see [`Record::auto_done`]).
    auto_done: Vec<String>,
}

impl Cx {
    /// Write one JSON frame to the child's stdin.
    pub fn write(&mut self, v: Value) {
        self.writes.push((v, None));
    }
    /// A user prompt: kept as a preview so a write lost in a crash is reported.
    pub fn write_prompt(&mut self, v: Value, preview: &str) {
        self.writes
            .push((v, Some(preview.chars().take(60).collect())));
    }
    pub fn render(&mut self, s: impl Into<String>) {
        self.view.push(View::Text(s.into()));
    }
    /// A tool call started (`label`: its one-line summary).
    pub fn tool_start(&mut self, id: &str, label: impl Into<String>) {
        self.view.push(View::ToolStart {
            id: id.to_string(),
            label: label.into(),
        });
    }
    /// A tool call ended, with what it changed (`diff`) and printed (`output`).
    pub fn tool_end(
        &mut self,
        id: &str,
        status: ToolStatus,
        output: Option<String>,
        diff: Option<String>,
        exit_code: Option<i64>,
    ) {
        self.view.push(View::ToolEnd {
            id: id.to_string(),
            status,
            output,
            diff,
            exit_code,
        });
    }
    /// A hook-vocabulary signal (04 §3.3) for the run.
    pub fn signal(&mut self, event: &str, payload: Value) {
        self.signals.push((event.to_string(), payload));
    }
    /// A server→client request that needs a decision.
    pub fn open(&mut self, it: Interaction) {
        self.opens.push(it);
    }
    /// The harness withdrew or resolved a request itself.
    pub fn resolved(&mut self, native_ref: String) {
        self.resolved.push(native_ref);
    }
    /// Token usage: a delta (`false`) or the session total (`true`).
    pub fn usage(&mut self, u: RunUsage, total: bool) {
        self.usage.push((u, total));
    }
    /// An ACP `terminal/*` request (live only; the session answers it, maybe later).
    pub fn terminal(&mut self, native_ref: String, r: acp_term::Req) {
        self.terminal.push((native_ref, r));
    }
    /// A rate-limit snapshot (Codex `account/rateLimits/updated`).
    pub fn rate_limit(&mut self, l: RateLimitInfo) {
        self.rate_limits.push(l);
    }
    /// The harness reported (or confirmed) its session id.
    pub fn session(&mut self, id: &str) {
        self.session = Some(id.to_string());
    }
    /// An automatic request's side effect was carried out (persisted before its response is
    /// written).
    pub fn auto_done(&mut self, native_ref: String) {
        self.auto_done.push(native_ref);
    }
    fn live() -> Cx {
        Cx {
            live: true,
            ..Cx::default()
        }
    }
}

pub trait Adapter: Send {
    /// Handshake for a freshly spawned process.
    fn start(&mut self, cx: &mut Cx);
    /// One JSONL frame from the child's stdout.
    fn on_frame(&mut self, cx: &mut Cx, v: &Value);
    /// One frame Vibeke wrote to the child's stdin, from the journal (live echo or replay):
    /// requests in flight, answered requests and prompts are learned from here, never from the
    /// intent to write, so replay and live operation agree.
    fn on_sent(&mut self, cx: &mut Cx, v: &Value);
    fn prompt(&mut self, cx: &mut Cx, text: &str, mode: PromptMode) -> Result<(), String>;
    fn interrupt(&mut self, cx: &mut Cx);
    /// Answer a pending request; false when it is no longer pending.
    fn answer(&mut self, cx: &mut Cx, native_ref: &str, it: &Interaction, a: &Answer) -> bool;
    /// Requests still waiting for Vibeke (journal order).
    fn pending(&self) -> Vec<Pending>;
    /// After a journal replay: finish an interrupted handshake, answer automatic requests and
    /// query the harness where the journal cannot be trusted to be complete (`gap`).
    fn reconcile(&mut self, cx: &mut Cx, gap: bool);
    /// The session is established (prompts can be sent).
    fn ready(&self) -> bool;
    /// A turn is in progress.
    fn busy(&self) -> bool;
    /// The harness queues a follow-up sent mid-turn itself. When false, the session keeps
    /// follow-ups in [`Record::queued`] and sends them once the turn ends.
    fn native_follow_up(&self) -> bool {
        true
    }
    /// Automatic requests whose side effect already happened (from [`Record::auto_done`]).
    fn set_auto_done(&mut self, _done: &[String]) {}
}

/// Line buffer of one journal stream.
#[derive(Default)]
struct LineBuf {
    buf: Vec<u8>,
    /// Discard bytes up to the first newline (the ring started mid-frame).
    skip_partial: bool,
}

/// The adapter of one pipe pane plus its journal bookkeeping. Lives across holder reconnects.
pub struct Session {
    pane: String,
    h: Option<Harness>,
    pub rec: Record,
    adapter: Box<dyn Adapter>,
    /// Journal bytes seen (any stream).
    seen: u64,
    lines: [LineBuf; 3],
    replaying: bool,
    /// The replay follows a server restart (the previous server's unacked inputs are judged
    /// against the journal); false for a reconnect, whose ledger resends them.
    restart: bool,
    gap: bool,
    /// `InputWritten` markers seen in the journal (decides `input_unconfirmed` after a crash).
    written: HashSet<u64>,
    /// Input id → (interaction, idempotency key) whose ack confirms delivery.
    deliveries: HashMap<u64, (String, String)>,
    /// Commands that arrived during a replay.
    queued: Vec<Cmd>,
    editor: String,
    /// The in-pane numbered prompt shows this request.
    prompt_ref: Option<String>,
    /// Native refs of server→client requests the journal showed (live or replayed): evidence
    /// that a request which is no longer pending was answered. After a truncated replay a
    /// request missing from here proves nothing (04 §7.3).
    seen_requests: HashSet<String>,
    /// [`Self::dispatch_queued`] is running (its prompts re-enter `apply`).
    dispatching: bool,
    /// Signals emitted since the last persist of `processed`.
    dirty: bool,
    /// What the pane shows, kept for redraws (`ctrl+o`).
    transcript: transcript::Transcript,
    /// ACP terminals of this run (shared with their watcher tasks).
    terms: acp_term::Terms,
    /// `terminal/wait_for_exit` requests waiting: (terminal id, request id).
    term_waits: Vec<(String, Value)>,
    /// Tests: start terminal commands with this instead of a pane.
    #[cfg(test)]
    pub term_spawn: Option<TestSpawn>,
    /// Tests: stop right after a terminal command started, before its pane is recorded.
    #[cfg(test)]
    pub crash_after_spawn: bool,
}

#[cfg(test)]
pub type TestSpawn =
    Box<dyn FnMut(&std::path::Path, Vec<String>, String) -> Result<String, String> + Send>;

fn stream_ix(s: Stream) -> usize {
    match s {
        Stream::Stdin => 1,
        Stream::Stderr => 2,
        _ => 0,
    }
}

impl Session {
    pub fn new(pane: &str, rec: Record) -> Session {
        let mut adapter = rec.kind.adapter(&rec);
        adapter.set_auto_done(&rec.auto_done);
        Session {
            pane: pane.to_string(),
            h: Harness::from_id(&rec.harness),
            rec,
            adapter,
            seen: 0,
            lines: Default::default(),
            replaying: false,
            restart: false,
            gap: false,
            written: HashSet::new(),
            deliveries: HashMap::new(),
            queued: vec![],
            editor: String::new(),
            prompt_ref: None,
            seen_requests: HashSet::new(),
            dispatching: false,
            dirty: false,
            transcript: transcript::Transcript::default(),
            terms: Default::default(),
            term_waits: vec![],
            #[cfg(test)]
            term_spawn: None,
            #[cfg(test)]
            crash_after_spawn: false,
        }
    }

    pub fn load(server: &Server, pane: &str) -> Option<Session> {
        Record::load(server, pane).map(|r| Session::new(pane, r))
    }

    /// Journal offset this session has consumed (a reconnect attaches from here).
    pub fn seen(&self) -> u64 {
        self.seen
    }

    /// A restarted server: rebuild from the journal with a fresh adapter.
    pub fn begin_replay(&mut self) {
        self.adapter = self.rec.kind.adapter(&self.rec);
        self.adapter.set_auto_done(&self.rec.auto_done);
        self.seen_requests.clear();
        self.seen = 0;
        self.lines = Default::default();
        self.replaying = true;
        self.restart = true;
        self.gap = false;
        self.written.clear();
        self.prompt_ref = None;
        self.transcript = transcript::Transcript::default();
    }

    /// Reconnect to the same holder (no restart): continue from [`Self::seen`].
    pub fn begin_reconnect(&mut self) {
        self.replaying = true;
        self.restart = false;
    }

    /// The journal no longer reaches back to the session start.
    pub fn on_gap(&mut self, available_from: u64) {
        self.gap = true;
        self.seen = self.seen.max(available_from);
        for l in &mut self.lines {
            l.buf.clear();
            l.skip_partial = true;
        }
    }

    /// Carry out an ACP `terminal/*` request; its response is written now or when the
    /// terminal exits (`wait_for_exit`).
    fn on_terminal(
        &mut self,
        server: &Arc<Server>,
        native_ref: &str,
        r: &acp_term::Req,
    ) -> Vec<Act> {
        let before = self.rec.terminals.clone();
        let resp = {
            // The record as it is, with the terminals under change swapped in on each persist.
            let snapshot = self.rec.clone();
            let pane = self.pane.clone();
            let mut persist = |t: &[acp_term::Saved]| {
                let mut rec = snapshot.clone();
                rec.terminals = t.to_vec();
                rec.try_persist(server, &pane)
            };
            #[cfg(test)]
            let test_spawn = self.term_spawn.take();
            #[cfg(test)]
            let mut test_spawn = test_spawn;
            let owner = self.pane.clone();
            let resp = {
                let mut spawn = |dir: &std::path::Path, argv: Vec<String>, title: String| {
                    #[cfg(test)]
                    if let Some(f) = test_spawn.as_mut() {
                        return f(dir, argv, title);
                    }
                    server
                        .split_pane(
                            &owner,
                            vk_proto::layout::Direction::Down,
                            0.5,
                            Some(&dir.to_string_lossy()),
                            Some(argv),
                            Some(title),
                            None,
                            &format!("agent:{owner}"),
                        )
                        .map(|p| p.id)
                        .map_err(|e| e.to_string())
                };
                let mut cx = acp_term::Ctx {
                    server,
                    owner: &self.pane,
                    cwd: &self.rec.cwd,
                    isolated: self.rec.isolated,
                    saved: &mut self.rec.terminals,
                    terms: &self.terms,
                    waits: &mut self.term_waits,
                    persist: &mut persist,
                    spawn: &mut spawn,
                    #[cfg(test)]
                    crash_after_spawn: self.crash_after_spawn,
                };
                acp_term::handle(&mut cx, native_ref, r)
            };
            #[cfg(test)]
            {
                self.term_spawn = test_spawn;
            }
            resp
        };
        if self.rec.terminals != before {
            // Persisted before the response is written (a restart must find the pane).
            self.dirty = true;
            self.flush(server);
        }
        match resp {
            Some(v) => vec![self.write(server, &v, None)],
            None => vec![],
        }
    }

    /// Transcript text from the session itself (prompts, warnings).
    fn say(&mut self, s: impl Into<String>) -> Act {
        Act::Render(self.transcript.push(View::Text(s.into())))
    }

    pub fn on_input_written(&mut self, id: u64) {
        self.written.insert(id);
    }

    /// Fresh process: run the handshake.
    pub fn start(&mut self, server: &Arc<Server>) -> Vec<Act> {
        let mut cx = Cx::live();
        cx.render(format!(
            "vibeke · {} headless ({}) · {}\n",
            self.rec.harness,
            self.rec.kind.integration().trim_start_matches("headless:"),
            self.rec.cwd
        ));
        self.adapter.start(&mut cx);
        let acts = self.apply(server, cx, true);
        self.flush(server);
        acts
    }

    /// Persist the record if anything changed: always before the acts of a step are carried
    /// out, so what they depend on (queued prompts, unacked previews, completed side effects)
    /// survives a crash.
    fn flush(&mut self, server: &Server) {
        if std::mem::take(&mut self.dirty) {
            self.rec.persist(server, &self.pane);
        }
    }

    pub fn on_output(
        &mut self,
        server: &Arc<Server>,
        stream: Stream,
        offset: u64,
        bytes: &[u8],
    ) -> Vec<Act> {
        let end = offset + bytes.len() as u64;
        if end <= self.seen {
            return vec![];
        }
        let skip = self.seen.saturating_sub(offset) as usize;
        let data = &bytes[skip.min(bytes.len())..];
        let base = offset + skip as u64;
        self.seen = end;
        let mut acts = Vec::new();
        let ix = stream_ix(stream);
        let mut start = 0;
        for (i, b) in data.iter().enumerate() {
            if *b != b'\n' {
                continue;
            }
            let line_end = base + i as u64 + 1;
            let mut line = std::mem::take(&mut self.lines[ix].buf);
            line.extend_from_slice(&data[start..i]);
            start = i + 1;
            if std::mem::take(&mut self.lines[ix].skip_partial) {
                continue;
            }
            acts.extend(self.on_line(server, stream, &line, line_end));
        }
        self.lines[ix].buf.extend_from_slice(&data[start..]);
        if !self.replaying {
            self.rec.processed = self.rec.processed.max(self.seen);
            if std::mem::take(&mut self.dirty) {
                self.rec.persist(server, &self.pane);
            }
        }
        acts
    }

    fn on_line(&mut self, server: &Arc<Server>, stream: Stream, line: &[u8], end: u64) -> Vec<Act> {
        let live = !self.replaying || end > self.rec.processed;
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches('\r');
        let mut cx = Cx {
            live: !self.replaying,
            ..Cx::default()
        };
        match stream {
            Stream::Stderr => {
                if !text.trim().is_empty() {
                    cx.render(format!("\x1b[2m{text}\x1b[0m\n"));
                }
            }
            Stream::Stdin => {
                if let Ok(v) = serde_json::from_str::<Value>(text) {
                    self.adapter.on_sent(&mut cx, &v);
                }
            }
            _ => match serde_json::from_str::<Value>(text) {
                Ok(v) => self.adapter.on_frame(&mut cx, &v),
                Err(_) if !text.trim().is_empty() => cx.render(format!("{text}\n")),
                Err(_) => {}
            },
        }
        self.apply(server, cx, live)
    }

    /// Execute one adapter step. `live`: emit events (otherwise state and transcript only).
    fn apply(&mut self, server: &Arc<Server>, cx: Cx, live: bool) -> Vec<Act> {
        let mut acts: Vec<Act> = cx
            .view
            .into_iter()
            .map(|v| Act::Render(self.transcript.push(v)))
            .collect();
        for r in cx.auto_done {
            if !self.rec.auto_done.contains(&r) {
                self.rec.auto_done.push(r);
            }
            let n = self.rec.auto_done.len();
            if n > AUTO_DONE_MAX {
                self.rec.auto_done.drain(..n - AUTO_DONE_MAX);
            }
            self.adapter.set_auto_done(&self.rec.auto_done);
            self.dirty = true;
        }
        for it in &cx.opens {
            if let Some(r) = &it.native_ref {
                self.seen_requests.insert(r.clone());
            }
        }
        if let Some(s) = cx.session
            && self.rec.session.as_deref() != Some(s.as_str())
        {
            self.rec.session = Some(s);
            self.dirty = true;
        }
        if live && let Some(h) = self.h {
            for (event, p) in cx.signals {
                self.dirty = true;
                signal(server, &self.pane, h, &event, &p);
            }
            for (u, total) in cx.usage {
                // A delta must not be counted again after a restart: persist `processed`.
                self.dirty = true;
                if let Some(run) = server.with_core(|c| c.run(&self.rec.run).cloned()) {
                    usage::from_headless(server, &run, u, total);
                }
            }
            for l in cx.rate_limits {
                if let Some(run) = server.with_core(|c| c.run(&self.rec.run).cloned()) {
                    usage::from_headless_limit(server, &run, l);
                }
            }
            for r in cx.resolved {
                self.dirty = true;
                if self.prompt_ref.as_deref() == Some(r.as_str()) {
                    self.prompt_ref = None;
                }
                if let Some(id) = interaction_for(server, &self.rec.run, &r) {
                    resolve(server, &id, InteractionStatus::ResolvedElsewhere, "harness");
                }
            }
        }
        if !self.replaying {
            for it in cx.opens {
                self.dirty = true;
                acts.extend(self.open(server, it));
            }
        }
        if !self.replaying {
            for (v, preview) in cx.writes {
                acts.push(self.write(server, &v, preview));
            }
            for (native_ref, r) in cx.terminal {
                acts.extend(self.on_terminal(server, &native_ref, &r));
            }
        }
        if !self.replaying {
            acts.extend(self.dispatch_queued(server));
        }
        acts
    }

    /// Hand queued prompts to the adapter, in order, while it can take them: once the session
    /// is ready, and a follow-up only when no turn is running and no earlier prompt is still
    /// on its way (one follow-up starts one turn).
    fn dispatch_queued(&mut self, server: &Arc<Server>) -> Vec<Act> {
        if self.dispatching {
            return vec![];
        }
        self.dispatching = true;
        let mut acts = Vec::new();
        while let Some(q) = self.rec.queued.first().cloned() {
            let follow = q.mode == PromptMode::FollowUp;
            if !self.adapter.ready()
                || (follow && (self.adapter.busy() || !self.rec.unacked.is_empty()))
            {
                break;
            }
            self.rec.queued.remove(0);
            self.dirty = true;
            let mode = if follow { PromptMode::Send } else { q.mode };
            acts.extend(self.prompt(server, &q.text, mode));
            if follow {
                break;
            }
        }
        self.dispatching = false;
        acts
    }

    fn write(&mut self, server: &Server, v: &Value, preview: Option<String>) -> Act {
        let id = server.next_internal_input_id();
        self.write_id(id, v, preview)
    }

    fn write_id(&mut self, id: u64, v: &Value, preview: Option<String>) -> Act {
        let mut bytes = serde_json::to_vec(v).unwrap_or_default();
        bytes.push(b'\n');
        if let Some(p) = preview {
            self.rec.unacked.push((id, p));
            self.dirty = true;
        }
        Act::Write { id, bytes }
    }

    /// Open (or re-attach) the interaction for a pending request; deliver at once when a
    /// decision is already recorded (policy, or answered before a crash and never written).
    fn open(&mut self, server: &Arc<Server>, it: Interaction) -> Vec<Act> {
        let Some(h) = self.h else { return vec![] };
        let native_ref = it.native_ref.clone().unwrap_or_default();
        let focused = server.pane_focused_by_any(&self.pane);
        match open_interaction(server, &self.pane, &self.rec.run, h, it, focused) {
            Opened::Deliver(id, key) => self.deliver(server, &id, &native_ref, &key),
            Opened::Open(it) => {
                self.prompt_ref = Some(native_ref);
                vec![self.say(numbered_prompt(&it))]
            }
            Opened::Done => vec![],
        }
    }

    fn deliver(
        &mut self,
        server: &Arc<Server>,
        interaction: &str,
        native_ref: &str,
        key: &str,
    ) -> Vec<Act> {
        let Some(it) = server.with_core(|c| c.interaction(interaction).cloned()) else {
            return vec![];
        };
        let Some(answer) = it.answer.clone() else {
            return vec![];
        };
        // Already on its way (a reconnect re-opens a request whose response the holder has not
        // journaled yet): the ledger resends that input under its id; never write a second one.
        if self.deliveries.values().any(|(i, _)| i == interaction) {
            return vec![];
        }
        let mut cx = Cx::live();
        if !self.adapter.answer(&mut cx, native_ref, &it, &answer) {
            set_delivery(
                server,
                interaction,
                DeliveryState::Failed,
                Some("the request is no longer pending".into()),
            );
            return vec![];
        }
        if self.prompt_ref.as_deref() == Some(native_ref) {
            self.prompt_ref = None;
        }
        set_delivery(server, interaction, DeliveryState::Delivering, None);
        cx.render(format!(
            "✓ {} (answered from Vibeke)\n",
            answer_label(&it, &answer)
        ));
        let mut acts: Vec<Act> = cx
            .view
            .drain(..)
            .map(|v| Act::Render(self.transcript.push(v)))
            .collect();
        let mut first = true;
        for (v, preview) in std::mem::take(&mut cx.writes) {
            if first {
                // The response's input id derives from the decision's idempotency key, so a
                // retry of the same decision (after a reconnect or restart) is the same input
                // and the holder's dedupe collapses it (04 §7.3).
                let id = delivery_input_id(key);
                self.deliveries
                    .insert(id, (interaction.to_string(), key.to_string()));
                acts.push(self.write_id(id, &v, preview));
                first = false;
            } else {
                acts.push(self.write(server, &v, preview));
            }
        }
        acts
    }

    /// The journal replay finished: report lost inputs, reconcile, run queued commands.
    pub fn replay_done(&mut self, server: &Arc<Server>) -> Vec<Act> {
        self.replaying = false;
        let mut acts = Vec::new();
        // Inputs the old server wrote but never saw acked: confirmed iff the journal has their
        // `InputWritten` marker (01 §1.2: lost input is reported, never resent).
        let unacked = if std::mem::take(&mut self.restart) {
            std::mem::take(&mut self.rec.unacked)
        } else {
            vec![]
        };
        for (id, preview) in unacked {
            if self.written.contains(&id) {
                continue;
            }
            input_unconfirmed(
                server,
                &self.pane,
                &self.rec.run,
                id,
                &preview,
                "server_restarted",
            );
            acts.push(self.say(format!(
                "⚠ not confirmed: \"{preview}\" may not have reached {} (input_unconfirmed); send it again if needed\n",
                self.rec.harness
            )));
        }
        // ACP terminals created before the restart: watch their panes again first, so the
        // requests reconciled below find them.
        acp_term::rewatch(server, &self.pane, &self.rec.terminals, &self.terms);
        let mut cx = Cx::live();
        self.adapter.reconcile(&mut cx, self.gap);
        acts.extend(self.apply(server, cx, true));
        // Pending requests: re-open (deduped by native ref) or deliver a recorded decision.
        let pending = self.adapter.pending();
        let refs: HashSet<String> = pending.iter().map(|p| p.native_ref.clone()).collect();
        for p in pending {
            if let Some(it) = p.interaction {
                acts.extend(self.open(server, it));
            }
        }
        // Interactions whose request is no longer pending. Only evidence counts: the journal
        // showed the request and then no longer has it waiting, or the journal reaches back to
        // the session start. After a truncated replay (`gap`) a request the journal never
        // showed proves nothing: an answer stays `delivery_unknown` and an unanswered request
        // stays open (04 §7.3 rule 2).
        let in_flight: HashSet<String> = self.deliveries.values().map(|(i, _)| i.clone()).collect();
        let stale: Vec<Interaction> = server.with_core(|c| {
            c.model
                .interactions
                .iter()
                .filter(|i| {
                    i.run == self.rec.run
                        && i.native_ref
                            .as_deref()
                            .is_some_and(|r| r.starts_with("rpc:"))
                        && !refs.contains(i.native_ref.as_deref().unwrap_or(""))
                        && !in_flight.contains(&i.id)
                        && (i.status == InteractionStatus::Open
                            || matches!(
                                i.delivery,
                                DeliveryState::DecisionRecorded
                                    | DeliveryState::Delivering
                                    | DeliveryState::DeliveryUnknown
                            ))
                })
                .cloned()
                .collect()
        });
        for it in stale {
            let r = it.native_ref.as_deref().unwrap_or("");
            let evidence = !self.gap || self.seen_requests.contains(r);
            match (it.status == InteractionStatus::Open, evidence) {
                (true, true) => resolve(
                    server,
                    &it.id,
                    InteractionStatus::ResolvedElsewhere,
                    "request no longer pending after restart",
                ),
                // The response is in the journal: it was written before the crash.
                (false, true) => set_delivery(server, &it.id, DeliveryState::Delivered, None),
                (true, false) => {}
                (false, false) => {
                    set_delivery(
                        server,
                        &it.id,
                        DeliveryState::DeliveryUnknown,
                        Some(
                            "journal_truncated: the journal no longer shows this request; \
                             delivery could not be confirmed"
                                .into(),
                        ),
                    );
                    acts.push(self.say(format!(
                        "⚠ could not confirm that the answer to \"{}\" reached {} (delivery_unknown); check the harness and answer again if it is still waiting\n",
                        it.title, self.rec.harness
                    )));
                }
            }
        }
        self.rec.processed = self.rec.processed.max(self.seen);
        self.rec.persist(server, &self.pane);
        self.dirty = false;
        for c in std::mem::take(&mut self.queued) {
            acts.extend(self.on_cmd(server, c));
        }
        self.flush(server);
        acts
    }

    pub fn on_cmd(&mut self, server: &Arc<Server>, c: Cmd) -> Vec<Act> {
        if self.replaying {
            self.queued.push(c);
            return vec![];
        }
        let acts = match c {
            Cmd::Attach(_) => vec![],
            Cmd::Prompt { text, mode, ack } => {
                // Not ready yet, behind earlier queued prompts, or a follow-up the harness does
                // not queue itself: keep it in the record (persisted before the ack below).
                let queue = !self.adapter.ready()
                    || !self.rec.queued.is_empty()
                    || (mode == PromptMode::FollowUp
                        && self.adapter.busy()
                        && !self.adapter.native_follow_up());
                let acts = if queue {
                    let mut acts = Vec::new();
                    if self.adapter.ready() {
                        acts.push(self.say(format!("› (queued) {text}\n")));
                    }
                    self.rec.queued.push(Queued { text, mode });
                    self.dirty = true;
                    acts.extend(self.dispatch_queued(server));
                    acts
                } else {
                    self.prompt(server, &text, mode)
                };
                self.flush(server);
                if let Some(a) = ack {
                    let _ = a.send(Ok(()));
                }
                acts
            }
            Cmd::Interrupt => {
                let mut cx = Cx::live();
                self.adapter.interrupt(&mut cx);
                self.apply(server, cx, true)
            }
            Cmd::Answer {
                interaction,
                native_ref,
                key,
            } => self.deliver(server, &interaction, &native_ref, &key),
            Cmd::TerminalExited(tid) => acp_term::exited(&self.terms, &mut self.term_waits, &tid)
                .into_iter()
                .map(|v| self.write(server, &v, None))
                .collect(),
        };
        self.flush(server);
        acts
    }

    fn prompt(&mut self, server: &Arc<Server>, text: &str, mode: PromptMode) -> Vec<Act> {
        let mut cx = Cx::live();
        match self.adapter.prompt(&mut cx, text, mode) {
            Ok(()) => {}
            Err(e) => cx.render(format!("! {e}\n")),
        }
        self.apply(server, cx, true)
    }

    /// Keystrokes typed in the pane: a minimal line editor (never raw to the child's stdin).
    pub fn on_keys(&mut self, server: &Arc<Server>, bytes: &[u8]) -> Vec<Act> {
        let text = String::from_utf8_lossy(bytes)
            .replace("\x1b[200~", "")
            .replace("\x1b[201~", "");
        let b = text.as_bytes();
        let mut acts = Vec::new();
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'\r' | b'\n' => {
                    let line = std::mem::take(&mut self.editor);
                    // The typed characters were echoed as they came; keep the line for redraws.
                    self.transcript.push(View::Text(format!("{line}\n")));
                    acts.push(Act::Render("\n".into()));
                    acts.extend(self.submit_line(server, line.trim()));
                    if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') {
                        i += 1;
                    }
                }
                0x1b if !matches!(b.get(i + 1), Some(b'[' | b'O')) => {
                    if self.adapter.busy() {
                        acts.extend(self.on_cmd(server, Cmd::Interrupt));
                        acts.push(self.say("\n(interrupt requested)\n"));
                    }
                }
                0x1b => {
                    i += 2;
                    while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                        i += 1;
                    }
                }
                0x03 => {
                    if self.adapter.busy() {
                        acts.extend(self.on_cmd(server, Cmd::Interrupt));
                    }
                    self.editor.clear();
                    acts.push(self.say("^C\n› "));
                }
                // ctrl+o: tool calls collapsed ⇄ expanded (the transcript is redrawn).
                0x0f => acts.push(Act::Render(self.transcript.toggle(&self.editor))),
                0x7f | 0x08 => {
                    if self.editor.pop().is_some() {
                        acts.push(Act::Render("\x08 \x08".into()));
                    }
                }
                c if c >= 0x20 => {
                    let start = i;
                    let mut end = i + 1;
                    while end < b.len() && (b[end] & 0xc0) == 0x80 {
                        end += 1;
                    }
                    let s = String::from_utf8_lossy(&b[start..end]).to_string();
                    self.editor.push_str(&s);
                    acts.push(Act::Render(s));
                    i = end - 1;
                }
                _ => {}
            }
            i += 1;
        }
        self.flush(server);
        acts
    }

    fn submit_line(&mut self, server: &Arc<Server>, line: &str) -> Vec<Act> {
        if line.is_empty() {
            return vec![];
        }
        // A pending request shown as a numbered prompt: the line picks an option.
        if let Some(r) = self.prompt_ref.clone()
            && let Some(id) = interaction_for(server, &self.rec.run, &r)
            && let Some(it) = server.with_core(|c| c.interaction(&id).cloned())
            && it.status == InteractionStatus::Open
        {
            let Some(answer) = answer_from_line(&it, line) else {
                return vec![self.say(numbered_prompt(&it))];
            };
            return match record_decision(server, &id, answer, "pane", None, None, None) {
                Ok(Some((_, key))) => self.deliver(server, &id, &r, &key),
                _ => vec![],
            };
        }
        self.on_cmd(
            server,
            Cmd::Prompt {
                text: line.to_string(),
                mode: if self.adapter.busy() {
                    PromptMode::FollowUp
                } else {
                    PromptMode::Send
                },
                ack: None,
            },
        )
    }

    /// Holder ack for one of this session's writes.
    pub fn on_ack(&mut self, server: &Arc<Server>, id: u64, status: InputStatus) -> Vec<Act> {
        let mut acts = Vec::new();
        if let Some(pos) = self.rec.unacked.iter().position(|(i, _)| *i == id) {
            let (_, preview) = self.rec.unacked.remove(pos);
            if !matches!(status, InputStatus::Written | InputStatus::Duplicate) {
                input_unconfirmed(
                    server,
                    &self.pane,
                    &self.rec.run,
                    id,
                    &preview,
                    "not_written",
                );
                acts.push(self.say(format!("⚠ not confirmed: \"{preview}\" ({status:?})\n")));
            }
            self.dirty = true;
        }
        if let Some((interaction, _key)) = self.deliveries.remove(&id) {
            match status {
                InputStatus::Written | InputStatus::Duplicate => {
                    set_delivery(server, &interaction, DeliveryState::Delivered, None)
                }
                InputStatus::ChildExited => set_delivery(
                    server,
                    &interaction,
                    DeliveryState::Failed,
                    Some("the harness exited".into()),
                ),
                InputStatus::Failed | InputStatus::Unconfirmed => set_delivery(
                    server,
                    &interaction,
                    DeliveryState::DeliveryUnknown,
                    Some(format!("{status:?}")),
                ),
            }
        }
        if !self.replaying {
            // A follow-up may have been waiting for this write to land.
            acts.extend(self.dispatch_queued(server));
        }
        self.flush(server);
        acts
    }

    /// The child exited: the run ends (04 §2.5 rule 1).
    pub fn on_exit(&mut self, server: &Arc<Server>, code: Option<i32>, signal: Option<i32>) {
        if let Some(h) = self.h {
            self::signal(
                server,
                &self.pane,
                h,
                "SessionEnd",
                &json!({"reason": "process_exited", "code": code, "signal": signal}),
            );
        }
        let reason = match (code, signal) {
            (Some(0), _) => "exited".to_string(),
            (Some(c), _) => format!("exited:{c}"),
            (_, Some(s)) => format!("killed:{s}"),
            _ => "exited".to_string(),
        };
        // The record stays: `agent.resume` reads the ACP command from it.
        server.agents.end_run(server, &self.rec.run, &reason);
    }
}

/// Route a headless signal like a hook signal. `Identified` re-identifies the run (a session id
/// reported mid-turn) without touching its execution state, unlike `SessionStart`.
fn signal(server: &Arc<Server>, pane: &str, h: Harness, event: &str, p: &Value) {
    if event != "Identified" {
        return super::on_signal(server, pane, h, event, p);
    }
    let run = bound_run(server, pane, h);
    let sid = p.get("session_id").and_then(Value::as_str);
    let model = p.get("model").and_then(Value::as_str).map(str::to_string);
    update_run(server, &run.id, |r, tx| {
        if let Some(s) = sid
            && r.harness_session_id.as_deref() != Some(s)
        {
            r.harness_session_id = Some(s.to_string());
            r.resume_argv = h.resume_argv(s);
            tx.event(
                "agent.identified",
                json!({"run": r.id, "pane": r.pane}),
                json!({"harness_session_id": s, "transcript_path": null}),
            );
        }
        r.model = model.or(r.model.take());
    });
}

/// Holder input id of the response delivering decision `key` (an interaction's idempotency
/// key): stable across reconnects and restarts. Bit 63 marks internal inputs and bit 62 keeps
/// these apart from the counter-allocated ones ([`Server::next_internal_input_id`]).
pub fn delivery_input_id(key: &str) -> u64 {
    // FNV-1a: deterministic across processes and builds.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (1u64 << 63) | (1u64 << 62) | (h & ((1u64 << 62) - 1))
}

fn interaction_for(server: &Server, run: &str, native_ref: &str) -> Option<String> {
    server.with_core(|c| {
        c.model
            .interactions
            .iter()
            .rev()
            .find(|i| i.run == run && i.native_ref.as_deref() == Some(native_ref))
            .map(|i| i.id.clone())
    })
}

enum Opened {
    /// A decision is recorded but not delivered: deliver it now.
    Deliver(String, String),
    Open(Box<Interaction>),
    /// Already answered and delivered, or closed.
    Done,
}

/// Open the interaction for a headless request, deduped by `native_ref` (04 §7.3 rule 4).
/// Headless requests are always answered natively: there is no harness dialog to fall back to,
/// only the in-pane numbered prompt.
fn open_interaction(
    server: &Arc<Server>,
    pane: &str,
    run_id: &str,
    h: Harness,
    mut it: Interaction,
    focused: bool,
) -> Opened {
    let Some(run) = server.with_core(|c| c.run(run_id).cloned()) else {
        return Opened::Done;
    };
    it.run = run.id.clone();
    it.pane = pane.to_string();
    it.answer_channel = AnswerChannel::Native;
    it.answerable = true;
    it.source = StateSource::Structured;
    it.confidence = 1.0;
    it.gate = !focused;
    let policy = if it.kind == InteractionKind::Approval {
        match_policy(server, &it)
    } else {
        None
    };
    {
        let mut c = server.core.lock().unwrap();
        if let Some(existing) = c
            .model
            .interactions
            .iter()
            .rev()
            .find(|x| x.run == run.id && x.native_ref == it.native_ref)
            .cloned()
        {
            return match (existing.status, existing.delivery) {
                (InteractionStatus::Open, _) => Opened::Open(Box::new(existing)),
                (
                    InteractionStatus::Answered,
                    DeliveryState::DecisionRecorded
                    | DeliveryState::Delivering
                    | DeliveryState::DeliveryUnknown,
                ) => Opened::Deliver(
                    existing.id.clone(),
                    existing
                        .answer_key
                        .clone()
                        .unwrap_or_else(|| format!("{}:{}", existing.id, existing.decision_rev)),
                ),
                _ => Opened::Done,
            };
        }
        it.handle = c.next_interaction_handle();
        let mut tx = Tx::new();
        tx.counters = true;
        tx.event(
            "interaction.opened",
            json!({"interaction": it.id, "pane": pane, "run": run.id}),
            json!({"kind": it.kind.as_str(), "source": "structured", "confidence": 1.0, "gate": it.gate, "native_ref": it.native_ref, "risk": it.action.as_ref().map(|a| format!("{:?}", a.risk).to_lowercase())}),
        );
        tx.interaction(it.clone());
        if server.commit(&mut c, tx).is_err() {
            return Opened::Done;
        }
    }
    let _ = h;
    if let Some((effect, rule)) = policy {
        let answer = Answer {
            decision: Some(if effect == "allow" {
                Decision::Allow
            } else {
                Decision::Deny
            }),
            choices: vec![],
            text: Some(format!("{effect} by policy rule {rule}")),
        };
        if let Ok(Some((_, key))) =
            record_decision(server, &it.id, answer, "policy", None, None, None)
        {
            return Opened::Deliver(it.id.clone(), key);
        }
    }
    notify_interaction(server, &it, &run);
    Opened::Open(Box::new(it))
}

/// `interaction.answer` on a headless run: hand the recorded decision to the pane's adapter.
pub(super) fn deliver_answer(server: &Server, it: &Interaction, key: &str) -> bool {
    let Some(rt) = server.pane_rt(&it.pane) else {
        return false;
    };
    let Some(native_ref) = it.native_ref.clone() else {
        return false;
    };
    rt.send(crate::pane::PaneCmd::Headless(Cmd::Answer {
        interaction: it.id.clone(),
        native_ref,
        key: key.to_string(),
    }));
    true
}

fn input_unconfirmed(server: &Server, pane: &str, run: &str, id: u64, preview: &str, reason: &str) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(
        "pane.input_unconfirmed",
        json!({"pane": pane, "run": run}),
        json!({"input_id": id.to_string(), "reason": reason, "preview_chars": preview.chars().count()}),
    );
    let _ = server.commit(&mut c, tx);
}

/// A new [`Interaction`] skeleton for an adapter (the session fills run, pane and handle).
pub fn interaction(
    kind: InteractionKind,
    native_ref: String,
    title: &str,
    action: Option<ActionInfo>,
    questions: Vec<Question>,
) -> Interaction {
    Interaction {
        id: crate::core::ulid(),
        handle: String::new(),
        run: String::new(),
        pane: String::new(),
        kind,
        status: InteractionStatus::Open,
        title: title.to_string(),
        body_md: None,
        action,
        questions,
        plan_md: None,
        answer_channel: AnswerChannel::Native,
        native_ref: Some(native_ref),
        source: StateSource::Structured,
        confidence: 1.0,
        answerable: true,
        gate: false,
        decision_rev: 0,
        delivery: DeliveryState::None,
        delivery_error: None,
        answer: None,
        answered_by: None,
        answer_key: None,
        opened_at_ms: now_ms(),
        answered_at_ms: None,
    }
}

/// An approval for a tool call (risk scored like hook approvals, 04 §7.6).
pub fn approval(native_ref: String, tool: &str, input: &Value, title: Option<&str>) -> Interaction {
    let command = input
        .get("command")
        .and_then(|c| {
            c.as_str().map(str::to_string).or_else(|| {
                c.as_array().map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            })
        })
        .filter(|c| !c.is_empty());
    let paths: Vec<String> = ["file_path", "path"]
        .iter()
        .filter_map(|k| input.get(*k).and_then(Value::as_str).map(str::to_string))
        .collect();
    let summary = harness::tool_summary(tool, input);
    let (risk, reasons) = harness::risk(tool, command.as_deref(), &paths);
    let title = title
        .map(str::to_string)
        .unwrap_or_else(|| format!("{tool}: {summary}"));
    interaction(
        InteractionKind::Approval,
        native_ref,
        &title,
        Some(ActionInfo {
            tool: tool.to_string(),
            summary,
            command,
            paths,
            diff: None,
            risk,
            risk_reasons: reasons,
        }),
        vec![],
    )
}

/// `allow` / `allow_always` / `deny` of an answer (`None`: no decision given).
pub fn decision(a: &Answer) -> Option<Decision> {
    a.decision
}

/// The option chosen for question `q` (by id or label), else the free text.
pub fn choice(it: &Interaction, a: &Answer, q: &str) -> Option<String> {
    let opts = it.questions.iter().find(|x| x.id == q);
    a.choices
        .iter()
        .find(|(id, _)| id == q)
        .and_then(|(_, v)| v.first())
        .map(|c| {
            opts.and_then(|o| {
                o.options
                    .iter()
                    .find(|x| &x.id == c || &x.label == c)
                    .map(|x| x.label.clone())
            })
            .unwrap_or_else(|| c.clone())
        })
        .or_else(|| a.text.clone())
}

fn answer_label(it: &Interaction, a: &Answer) -> String {
    match a.decision {
        Some(Decision::Allow) => "allowed".into(),
        Some(Decision::AllowAlways) => "allowed always".into(),
        Some(Decision::Deny) => "denied".into(),
        None => it
            .questions
            .first()
            .and_then(|q| choice(it, a, &q.id))
            .unwrap_or_else(|| "answered".into()),
    }
}

/// The in-pane prompt for a pending request.
fn numbered_prompt(it: &Interaction) -> String {
    let mut s = format!("⏵ {}\n", it.title);
    if let Some(c) = it.action.as_ref().and_then(|a| a.command.as_deref()) {
        s.push_str(&format!("  $ {c}\n"));
    }
    match it.questions.first() {
        Some(q) if !q.options.is_empty() => {
            for (i, o) in q.options.iter().enumerate() {
                s.push_str(&format!("  {}. {}\n", i + 1, o.label));
            }
            s.push_str(&format!("Choose 1-{} then Enter: ", q.options.len()));
        }
        Some(_) => s.push_str("Type the answer then Enter: "),
        None => s.push_str("  1. Allow\n  2. Allow always\n  3. Deny\nChoose 1-3 then Enter: "),
    }
    s
}

fn answer_from_line(it: &Interaction, line: &str) -> Option<Answer> {
    match it.questions.first() {
        Some(q) if !q.options.is_empty() => {
            let n: usize = line.parse().ok()?;
            let o = q.options.get(n.checked_sub(1)?)?;
            Some(Answer {
                decision: None,
                choices: vec![(q.id.clone(), vec![o.id.clone()])],
                text: None,
            })
        }
        Some(_) => Some(Answer {
            decision: None,
            choices: vec![],
            text: Some(line.to_string()),
        }),
        None => {
            let d = match line {
                "1" | "y" | "yes" => Decision::Allow,
                "2" | "a" | "always" => Decision::AllowAlways,
                "3" | "n" | "no" => Decision::Deny,
                _ => return None,
            };
            Some(Answer {
                decision: Some(d),
                choices: vec![],
                text: None,
            })
        }
    }
}

// ---- launching ------------------------------------------------------------------------------

/// Harness binary for a headless launch: `agents.harness.<id>.bin` is not a config key, so the
/// name is resolved through the pane's PATH like a typed command.
fn launch_argv(
    h: Harness,
    kind: Kind,
    session: Option<&str>,
    resume: bool,
    args: &[String],
    acp_argv: &[String],
) -> Vec<String> {
    let bin = h.base().id().to_string();
    let mut v = match kind {
        Kind::StreamJson => {
            let mut v = vec![
                bin,
                "-p".into(),
                "--input-format".into(),
                "stream-json".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                "--permission-prompt-tool".into(),
                "stdio".into(),
            ];
            if let Some(s) = session {
                v.push(if resume { "--resume" } else { "--session-id" }.into());
                v.push(s.into());
            }
            v
        }
        Kind::AppServer => vec![bin, "app-server".into()],
        Kind::Rpc => {
            // omp's rpc-ui mode is RPC plus its own approval dialogs (04 §6.3).
            let mode = if h.base() == Harness::Omp {
                "rpc-ui"
            } else {
                "rpc"
            };
            let mut v = vec![bin.clone(), "--mode".into(), mode.into()];
            if let Some(s) = session {
                match (h.base(), resume) {
                    (Harness::Omp, true) => v.extend(["--resume".into(), s.into()]),
                    (_, true) => v.extend(["--session".into(), s.into()]),
                    (Harness::Pi, false) => v.extend(["--session-id".into(), s.into()]),
                    _ => {}
                }
            }
            v
        }
        Kind::Acp => acp_argv.to_vec(),
    };
    v.extend(args.iter().cloned());
    v
}

/// The launch argv of a run Vibeke isolates: Codex's own Seatbelt sandbox cannot nest inside
/// Vibeke's, so (as on the PTY path, 13 §3) it is switched off at the sandbox level with the
/// configured `agents.harness.codex.isolated_args`, inserted right after the binary. Approvals
/// stay as they are: the app-server still asks Vibeke.
fn isolated_argv(
    h: Harness,
    iso: Option<IsolationLevel>,
    mut argv: Vec<String>,
    cfg: &vk_config::Config,
) -> Vec<String> {
    if iso != Some(IsolationLevel::Sandbox) || h.base() != Harness::Codex {
        return argv;
    }
    let extra = cfg
        .agents
        .harness
        .get("codex")
        .and_then(|c| c.isolated_args.clone())
        .unwrap_or_default();
    if !argv.is_empty() {
        argv.splice(1..1, extra);
    }
    argv
}

/// The session's shared app-server socket when `agents.harness.codex.headless_shared` is on
/// (04 §6.2). Isolated runs never share: their app-server must run inside their box.
fn shared_codex_socket(
    server: &Server,
    h: Harness,
    kind: Kind,
    iso: Option<IsolationLevel>,
    cfg: &vk_config::Config,
) -> Option<std::path::PathBuf> {
    let shared = cfg
        .agents
        .harness
        .get("codex")
        .and_then(|c| c.headless_shared)
        .unwrap_or(false);
    (shared && kind == Kind::AppServer && h.base() == Harness::Codex && iso.is_none())
        .then(|| server.paths.runtime.join("codex-mux.sock"))
}

/// `agent.start {mode: "headless"}` (and `agent.resume` of a headless run): a new tab whose pane
/// runs the harness under a pipe-mode holder, driven by the in-server adapter.
pub(super) async fn start(server: &Arc<Server>, ctx: Option<&Ctx>, p: &Value) -> R {
    let (h, acp_argv) = if s(p, "acp").is_some() {
        let (h, argv) = super::acp::resolve(p)?;
        (h, argv)
    } else {
        let id = s(p, "harness").unwrap_or("claude");
        let h = Harness::from_id(id).ok_or_else(|| invalid(format!("unknown harness {id}")))?;
        (h, vec![])
    };
    let kind = Kind::for_harness(h).ok_or_else(|| {
        err(
            ErrorKind::Unsupported,
            format!("{} has no headless mode", h.id()),
        )
    })?;
    if let Some(n) = s(p, "name") {
        validate_name(server, n)?;
    }
    let opts = crate::sandbox::LaunchOpts::from_params(p)?;
    let args: Vec<String> = p
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let resume = s(p, "resume").map(str::to_string);
    let session = resume.clone().or_else(|| match kind {
        Kind::StreamJson | Kind::Rpc if h.base() != Harness::Omp => h.base().preassign_session_id(),
        _ => None,
    });
    let base =
        ctx.and_then(|ctx| resolve_pane(server, ctx, s(p, "pane").or(s(p, "split_of"))).ok());
    let (ws, cwd) = match &base {
        Some(b) => (
            b.workspace.clone(),
            s(p, "cwd").map(str::to_string).or(b.cwd.clone()),
        ),
        None => {
            let ws = server
                .with_core(|c| c.model.workspaces.first().map(|w| w.id.clone()))
                .ok_or_else(|| invalid("no workspace"))?;
            (ws, s(p, "cwd").map(str::to_string))
        }
    };
    let cwd = cwd.unwrap_or_else(|| crate::paths::home().to_string_lossy().into_owned());
    // `isolate` / `network` go through the same isolation path as PTY agents (13 §3): the
    // run-scoped box exists before the pane spawns, so the harness never starts on the host.
    let pane_id = crate::core::ulid();
    let ws_task = server.with_core(|c| c.ws(&ws).and_then(|w| w.task.clone()));
    let iso =
        crate::sandbox::prepare_headless(server, &pane_id, ws_task.as_deref(), &cwd, h.id(), &opts)
            .await?;
    let mut cmd = vec![
        PIPE_ARGV0.to_string(),
        "/usr/bin/env".into(),
        // The pi extension and hook shims stay quiet: the adapter owns the stream.
        "VIBEKE_HEADLESS_OWNER=1".into(),
    ];
    if let Some(env) = p.get("env").and_then(Value::as_object) {
        for (k, v) in env {
            if let Some(v) = v.as_str()
                && !k.is_empty()
                && !k.contains('=')
            {
                cmd.push(format!("{k}={v}"));
            }
        }
    }
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    let argv = launch_argv(
        h,
        kind,
        session.as_deref(),
        resume.is_some(),
        &args,
        &acp_argv,
    );
    let argv = isolated_argv(h, iso, argv, &cfg);
    // A shared run's relay authenticates to the mux with a secret handed to it alone.
    let argv = match shared_codex_socket(server, h, kind, iso, &cfg) {
        Some(sock) => {
            let key = codex_mux::issue_key(&sock, &pane_id).map_err(internal)?;
            codex_mux::relay_argv(&server.opts.bin, &sock, &key, argv)
        }
        None => argv,
    };
    cmd.extend(argv.iter().cloned());
    let title = format!(
        "{} (headless)",
        s(p, "name").unwrap_or_else(|| h.id().trim_start_matches("acp:"))
    );
    let (_, pane) = match server.create_tab_as(
        &ws,
        Some(&cwd),
        Some(title),
        Some(cmd),
        None,
        Some(pane_id.clone()),
    ) {
        Ok(t) => t,
        Err(e) => {
            crate::sandbox::teardown(server, &format!("pane:{pane_id}"));
            return Err(internal(e));
        }
    };
    let rec = {
        let mut c = server.core.lock().unwrap();
        let mut run = new_run(
            &mut c,
            &pane.id,
            h,
            kind.integration(),
            StateSource::Structured,
            1.0,
        );
        run.name = s(p, "name").map(str::to_string);
        run.cwd = Some(cwd.clone());
        run.capabilities = kind.capabilities(h);
        if let Some(sid) = &session {
            run.harness_session_id = Some(sid.clone());
            run.resume_argv = h.resume_argv(sid);
        }
        if let Some(t) = s(p, "task") {
            run.task = Some(t.to_string());
        }
        let rec = Record {
            harness: h.id().to_string(),
            kind,
            run: run.id.clone(),
            cwd: cwd.clone(),
            session: session.clone(),
            resume: resume.is_some(),
            processed: 0,
            unacked: vec![],
            acp_argv: acp_argv.clone(),
            queued: vec![],
            auto_done: vec![],
            isolated: iso.is_some(),
            terminals: vec![],
        };
        let mut tx = Tx::new();
        tx.counters = true;
        tx.event(
            "agent.started",
            json!({"run": run.id, "pane": pane.id}),
            json!({"harness": h.id(), "via": "headless", "argv": argv, "resumed": resume.is_some()}),
        );
        tx.m.kv(KV_SCOPE, &pane.id, serde_json::to_string(&rec).ok());
        tx.run(run);
        server.commit(&mut c, tx).map_err(internal)?;
        rec
    };
    let run_id = rec.run.clone();
    let rt = server
        .pane_rt(&pane.id)
        .ok_or_else(|| internal("headless pane did not start"))?;
    rt.send(crate::pane::PaneCmd::Headless(Cmd::Attach(Box::new(rec))));
    if let Some(text) = s(p, "prompt") {
        rt.send(crate::pane::PaneCmd::Headless(Cmd::Prompt {
            text: text.to_string(),
            mode: PromptMode::Send,
            ack: None,
        }));
    }
    let timeout = u(p, "ready_timeout_ms").unwrap_or(30_000);
    let deadline = Instant::now() + Duration::from_millis(timeout);
    while Instant::now() < deadline {
        let st = server.with_core(|c| c.run(&run_id).map(|r| r.execution.value.clone()));
        match st {
            Some(Execution::Idle | Execution::Working | Execution::Exited) | None => break,
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let r = server.with_core(|c| c.run(&run_id).map(|r| run_json(c, r)));
    Ok(json!({"run": r, "pane": pane.id}))
}

/// `agent.resume` of a headless run: a new headless process continuing the session.
pub(super) async fn resume(server: &Arc<Server>, run: &AgentRun, pane: Option<&str>) -> R {
    let p = resume_params(server, run, pane)?;
    // With a destination pane, the new headless tab opens in that pane's workspace.
    let ctx = pane.map(|_| crate::drafts::user_ctx());
    start(server, ctx.as_ref(), &p).await
}

/// The `agent.start` parameters that continue `run` headless. With `pane` (e.g. a split's
/// destination) the run continues there: in that pane's workspace and cwd, not the original
/// cwd.
pub(super) fn resume_params(server: &Server, run: &AgentRun, pane: Option<&str>) -> R {
    let sid = run
        .harness_session_id
        .clone()
        .ok_or_else(|| err(ErrorKind::Unsupported, "no session to resume"))?;
    let cwd = match pane {
        Some(pid) => Some(
            server
                .pane_cwd(pid)
                .or_else(|| server.with_core(|c| c.pane(pid).and_then(|x| x.cwd.clone())))
                .ok_or_else(|| invalid(format!("pane {pid} has no working directory")))?,
        ),
        None => run.cwd.clone(),
    };
    let mut p = json!({
        "harness": run.harness,
        "resume": sid,
        "mode": "headless",
        "cwd": cwd,
    });
    if let Some(pid) = pane {
        p["pane"] = json!(pid);
    }
    if let Some(n) = &run.name
        && server.with_core(|c| {
            !c.model
                .runs
                .iter()
                .any(|r| r.name.as_deref() == Some(n) && r.ended_at_ms.is_none())
        })
    {
        p["name"] = json!(n);
    }
    if run.harness.starts_with("acp:") {
        let argv = Record::load(server, &run.pane)
            .map(|r| r.acp_argv)
            .unwrap_or_default();
        p["harness"] = json!(run.harness.trim_start_matches("acp:"));
        p["acp"] = json!(harness::shell_join(&argv));
    }
    Ok(p)
}

/// `agent.prompt` on a headless run.
pub(super) async fn prompt(server: &Server, run: &AgentRun, text: &str, mode: PromptMode) -> R {
    let rt = server
        .pane_rt(&run.pane)
        .ok_or_else(|| err(ErrorKind::Conflict, "the headless pane is not running"))?;
    let (tx, rx) = oneshot::channel();
    rt.send(crate::pane::PaneCmd::Headless(Cmd::Prompt {
        text: text.to_string(),
        mode,
        ack: Some(tx),
    }));
    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(Ok(()))) => Ok(json!({})),
        Ok(Ok(Err(e))) => Err(err(ErrorKind::Conflict, e)),
        _ => Err(err(ErrorKind::Timeout, "input_unconfirmed")
            .details(json!({"status": "input_unconfirmed"}))),
    }
}

/// `agent.interrupt` on a headless run.
pub(super) fn interrupt(server: &Server, run: &AgentRun) -> bool {
    match server.pane_rt(&run.pane) {
        Some(rt) => {
            rt.send(crate::pane::PaneCmd::Headless(Cmd::Interrupt));
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests;
