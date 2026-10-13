//! Background work of an agent run (`AgentRun::bg_tasks`): shells, monitors and subagents the
//! harness keeps running after its turn ended.
//!
//! Three kinds of input, best first:
//! - a whole set the harness reports ([`replace`]): Claude `Stop.background_tasks`, Claude
//!   stream-json `background_tasks_changed`, Codex `thread/backgroundTerminals/list`;
//! - start and end records from the transcript ([`started`], [`ended`]);
//! - child processes of the harness that outlive its turn ([`set_processes`]), for harnesses
//!   with neither. When the harness reports tool calls ([`observe_tool`]), only process groups
//!   that a shell tool call started, and that outlive the call, count.
//!
//! A reported set wins over transcript records it already covers: a start older than
//! `bg_level_ms` is history (the tailer replays whole files after a restart). Ends always apply.

use std::sync::Arc;

use serde_json::{Value, json};
use vk_proto::model::{AgentRun, BgTask, Execution, StateSource};

use super::{nested, update_run};
use crate::Server;
use crate::core::Tx;
use vk_store::now_ms;

/// Longest label kept (characters).
const LABEL_MAX: usize = 160;
/// A reported set reaches us a little after the harness took it: transcript starts this much
/// older than its arrival may postdate it, and ends this recent may not be in it yet.
const SKEW_MS: i64 = 2_000;
/// How long an ended task is remembered, so a set taken before it ended does not bring it back.
const ENDED_KEEP_MS: i64 = 10_000;

/// Recently ended tasks per run: (run, task id) → when the end was seen.
static ENDED: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<(String, String), i64>>,
> = std::sync::LazyLock::new(Default::default);

fn note_ended(run: &str, id: &str, now: i64) {
    let mut m = ENDED.lock().unwrap();
    m.retain(|_, t| now - *t < ENDED_KEEP_MS);
    m.insert((run.to_string(), id.to_string()), now);
}

fn ended_recently(run: &str, id: &str, now: i64) -> bool {
    ENDED
        .lock()
        .unwrap()
        .get(&(run.to_string(), id.to_string()))
        .is_some_and(|t| now - *t < ENDED_KEEP_MS)
}

/// A shortened, redacted label for a task.
pub fn label(s: &str) -> String {
    let first = s
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let red = vk_redact::redact(first);
    let mut out: String = red.chars().take(LABEL_MAX).collect();
    if red.chars().count() > LABEL_MAX {
        out.push('…');
    }
    out
}

/// Claude's `type` words (`local_bash` in the SDK, `shell` in hooks) as our kinds.
pub fn kind_of(native: &str) -> String {
    match native {
        "shell" | "local_bash" | "bash" => "shell",
        "subagent" | "local_agent" | "agent" | "remote_agent" => "subagent",
        "monitor" | "monitor_mcp" | "monitor_ws" => "monitor",
        "workflow" | "local_workflow" => "workflow",
        "teammate" | "in_process_teammate" => "teammate",
        other => other,
    }
    .to_string()
}

/// Claude `Stop.background_tasks` / stream-json `background_tasks_changed` entries as tasks.
/// Hook entries: `{id, type, status, description, command?, agent_type?, server?, tool?, name?}`;
/// stream-json entries: `{task_id, task_type, description, subagent_type?}`.
pub fn from_claude(list: &[Value]) -> Vec<BgTask> {
    let now = now_ms();
    list.iter()
        .filter_map(|t| {
            let s = |k: &str| t.get(k).and_then(Value::as_str);
            let id = s("id").or_else(|| s("task_id"))?;
            if matches!(
                s("status"),
                Some("completed" | "failed" | "killed" | "stopped")
            ) {
                return None;
            }
            let kind = kind_of(s("type").or_else(|| s("task_type")).unwrap_or("task"));
            let text = s("command")
                .or_else(|| s("description"))
                .or_else(|| s("name"))
                .or_else(|| s("agent_type"))
                .or_else(|| s("subagent_type"))
                .unwrap_or("");
            Some(BgTask {
                id: id.to_string(),
                kind,
                label: label(text),
                since_ms: now,
                source: StateSource::Structured,
                tool_use_id: s("tool_use_id").map(str::to_string),
            })
        })
        .collect()
}

fn ids(tasks: &[BgTask]) -> Vec<&str> {
    tasks.iter().map(|t| t.id.as_str()).collect()
}

/// Write the new set when it differs, with an `agent.bg_tasks_changed` event. When the set
/// empties while the run is idle and the harness does not wake itself for it (Claude starts a
/// turn for each notification), say the work finished.
fn store(server: &Arc<Server>, run: &str, f: impl FnOnce(&AgentRun) -> Option<(Vec<BgTask>, i64)>) {
    let finished = std::cell::Cell::new(None::<AgentRun>);
    update_run(server, run, |r, tx: &mut Tx| {
        // An ended run keeps no background work (`end_run_tx` cleared it).
        if r.ended_at_ms.is_some() {
            return;
        }
        let Some((tasks, level)) = f(r) else {
            return;
        };
        r.bg_level_ms = level;
        if tasks == r.bg_tasks {
            return;
        }
        let was = r.bg_tasks.len();
        r.bg_tasks = tasks;
        tx.event(
            "agent.bg_tasks_changed",
            json!({"run": r.id, "pane": r.pane}),
            json!({"count": r.bg_tasks.len(), "ids": ids(&r.bg_tasks)}),
        );
        if was > 0 && r.bg_tasks.is_empty() && r.execution.value == Execution::Idle {
            finished.set(Some(r.clone()));
        }
    });
    if let Some(r) = finished.take()
        && !wakes_itself(&r.harness)
        && !server.pane_focused_by_any(&r.pane)
    {
        server.notify(
            "agent_state",
            Some(&r.pane),
            &format!("{} finished its background work", r.label()),
            "",
            "low",
        );
    }
}

/// Harnesses that start a turn of their own when background work ends: their `Stop` says done.
fn wakes_itself(harness: &str) -> bool {
    super::harness::Harness::from_id(harness)
        .is_some_and(|h| h.family() == super::harness::Family::Claude)
}

/// A whole set reported by the harness (`source` structured). Keeps `since_ms`, `tool_use_id`
/// and a better label of tasks it already knew, and keeps process tasks (their own scan).
pub fn replace(server: &Arc<Server>, run: &str, tasks: Vec<BgTask>) {
    let now = now_ms();
    store(server, run, |r| {
        let mut next: Vec<BgTask> = tasks
            .into_iter()
            .filter(|t| !ended_recently(run, &t.id, now))
            .map(|mut t| {
                if let Some(old) = r.bg_tasks.iter().find(|o| o.id == t.id) {
                    t.since_ms = old.since_ms;
                    t.tool_use_id = t.tool_use_id.or_else(|| old.tool_use_id.clone());
                    if t.label.is_empty() {
                        t.label = old.label.clone();
                    }
                }
                t
            })
            .collect();
        let procs: Vec<BgTask> = r
            .bg_tasks
            .iter()
            .filter(|t| t.source == StateSource::Process && !reported_elsewhere(&next, t))
            .cloned()
            .collect();
        next.extend(procs);
        Some((next, now - SKEW_MS))
    });
}

/// A transcript record says `task` started at `task.since_ms`.
pub fn started(server: &Arc<Server>, run: &str, task: BgTask) {
    // An end recorded before its start (another source, or out of order) wins.
    if ended_recently(run, &task.id, now_ms()) {
        return;
    }
    store(server, run, |r| {
        // A reported set may have listed it first, without the tool call that started it.
        if let Some(i) = r.bg_tasks.iter().position(|t| t.id == task.id) {
            if r.bg_tasks[i].tool_use_id.is_some() || task.tool_use_id.is_none() {
                return None;
            }
            let mut next = r.bg_tasks.clone();
            next[i].tool_use_id = task.tool_use_id;
            next[i].since_ms = next[i].since_ms.min(task.since_ms);
            return Some((next, r.bg_level_ms));
        }
        if task.since_ms <= r.bg_level_ms {
            return None;
        }
        let mut next = r.bg_tasks.clone();
        next.push(task);
        Some((next, r.bg_level_ms))
    });
}

/// A transcript record (or a protocol event) says task `id` ended.
pub fn ended(server: &Arc<Server>, run: &str, id: &str) {
    note_ended(run, id, now_ms());
    store(server, run, |r| {
        if !r.bg_tasks.iter().any(|t| t.id == id) {
            return None;
        }
        let next = r.bg_tasks.iter().filter(|t| t.id != id).cloned().collect();
        Some((next, r.bg_level_ms))
    });
}

/// The process scan's current set ([`StateSource::Process`] tasks only).
pub fn set_processes(server: &Arc<Server>, run: &str, procs: Vec<BgTask>) {
    store(server, run, |r| {
        let mut next: Vec<BgTask> = r
            .bg_tasks
            .iter()
            .filter(|t| t.source != StateSource::Process)
            .cloned()
            .collect();
        for mut p in procs {
            // The harness reports this work itself (an omp async bash job): keep its entry.
            if reported_elsewhere(&next, &p) {
                continue;
            }
            if let Some(old) = r.bg_tasks.iter().find(|o| o.id == p.id) {
                p.since_ms = p.since_ms.min(old.since_ms);
            }
            next.push(p);
        }
        Some((next, r.bg_level_ms))
    });
}

/// Whether a process task is work a harness-reported task already covers: both came from the
/// same tool call.
fn reported_elsewhere(tasks: &[BgTask], p: &BgTask) -> bool {
    p.tool_use_id.as_ref().is_some_and(|id| {
        tasks
            .iter()
            .any(|t| t.source != StateSource::Process && t.tool_use_id.as_ref() == Some(id))
    })
}

/// Forget everything: the harness session started over or its process exited (its children
/// went with it, or are no longer its work).
pub fn clear(server: &Arc<Server>, run: &str) {
    let now = now_ms();
    forget_tools(run);
    store(server, run, |_| Some((vec![], now)));
}

/// When the run's harness process started (ms), once a hook has named it. Its background work
/// dies with it, so a transcript start older than this belongs to an earlier process (the
/// session was resumed, or the server restarted and replayed the file).
pub fn harness_started_ms(server: &Server, run: &AgentRun) -> Option<i64> {
    let (h, p) = server.agents.harness_proc(&run.pane)?;
    if h != run.harness {
        return None;
    }
    vk_hold::procinfo::start_unix_ms(p.start)
}

/// Whether a transcript start at `since_ms` is older than the harness process.
pub fn predates_process(server: &Server, run: &AgentRun, since_ms: i64) -> bool {
    harness_started_ms(server, run).is_some_and(|t0| since_ms < t0 - SKEW_MS)
}

/// Drop transcript tasks that started before the harness process, and raise `bg_level_ms` to
/// its start so a replayed start that arrives later is refused in the same update ([`started`]).
/// A replay can add such tasks before the first hook names the process.
pub(super) fn drop_older_than_process(server: &Arc<Server>, run: &AgentRun) {
    let Some(cutoff) = harness_started_ms(server, run).map(|t0| t0 - SKEW_MS) else {
        return;
    };
    let stale = |t: &BgTask| t.source == StateSource::Structured && t.since_ms < cutoff;
    if run.bg_level_ms >= cutoff && !run.bg_tasks.iter().any(stale) {
        return;
    }
    store(server, &run.id, |r| {
        let next: Vec<BgTask> = r.bg_tasks.iter().filter(|t| !stale(t)).cloned().collect();
        Some((next, r.bg_level_ms.max(cutoff)))
    });
}

/// Hook payloads: Claude `Stop` carries the session's whole background set.
pub fn observe_hook(server: &Arc<Server>, run: &AgentRun, event: &str, p: &Value) {
    match event {
        // Claude `Stop`, and `BgTasks` from headless adapters (Claude stream-json
        // `background_tasks_changed`, Codex `thread/backgroundTerminals/list`) and from the
        // pi/omp extension (omp async jobs, pi-subagents async runs).
        "Stop" | "BgTasks" => {
            if let Some(list) = p.get("background_tasks").and_then(Value::as_array) {
                replace(server, &run.id, from_claude(list));
            }
        }
        "SessionStart" => {
            // `startup`, `resume`, `clear`: a new process or conversation; nothing carries over.
            let src = p.get("source").and_then(Value::as_str).unwrap_or("");
            if src != "compact" {
                clear(server, &run.id);
            }
        }
        _ => {}
    }
}

/// How often the process scan runs.
const SCAN_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
/// A process younger than this is not counted yet (hook shims, short tool commands).
const MIN_AGE_MS: i64 = 3_000;
/// Process groups started this soon after the harness process are its own startup work (MCP
/// servers, language servers, extension watchers), unless a tool call claims them.
const STARTUP_MS: i64 = 5_000;
/// Shell tool calls remembered per run, besides those that still own a counted process group.
const TOOL_KEEP: usize = 64;

/// A shell-like tool call of a run, as the process scan matches it against process groups.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    /// The harness's call id (`tool_use_id`), when it sent one.
    pub id: Option<String>,
    /// The command the call runs; `None` when the harness truncated or withheld its input.
    pub command: Option<String>,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
}

/// Tool calls the scan knows of for one run. `seen` is set by any tool start, so a run whose
/// harness reports tool calls counts no process group that none of them started.
#[derive(Default)]
struct RunTools {
    seen: bool,
    calls: Vec<ToolCall>,
}

/// Run id → its recent shell tool calls. In memory only: after a server restart a run falls back
/// to the plain heuristic until its next tool call.
static TOOLS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, RunTools>>> =
    std::sync::LazyLock::new(Default::default);

fn forget_tools(run: &str) {
    TOOLS.lock().unwrap().remove(run);
}

/// The run's shell tool calls, or `None` when its harness has reported no tool call yet.
fn tools_of(run: &str) -> Option<Vec<ToolCall>> {
    let m = TOOLS.lock().unwrap();
    let t = m.get(run)?;
    t.seen.then(|| t.calls.clone())
}

/// Tool names that run shell commands, across harnesses (after hook mapping: pi/omp `bash`
/// arrives as `Bash`).
fn is_shell_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "bash"
            | "shell"
            | "sh"
            | "exec"
            | "exec_command"
            | "shell_command"
            | "local_shell"
            | "run_shell_command"
            | "terminal"
            | "run_terminal_cmd"
    )
}

/// The command in a tool's input: `command` (a string, or an argv array) or `cmd`.
fn command_input(input: &Value) -> Option<String> {
    let v = input.get("command").or_else(|| input.get("cmd"))?;
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => {
            let parts: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            (!parts.is_empty()).then(|| parts.join(" "))
        }
        _ => None,
    }
    .filter(|s| !s.trim().is_empty())
}

/// Remember shell tool calls of harnesses the process scan watches (`PreToolUse`, ended by
/// `PostToolUse`/`PostToolUseFailure`; pi/omp `ToolStarted`/`ToolEnded` arrive mapped to them).
pub fn observe_tool(run: &AgentRun, event: &str, p: &Value) {
    if reports_own(&run.harness) {
        return;
    }
    let now = now_ms();
    let id = p
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    match event {
        "PreToolUse" => {
            let tool = p.get("tool_name").and_then(Value::as_str).unwrap_or("");
            let command = p.get("tool_input").and_then(command_input);
            let mut m = TOOLS.lock().unwrap();
            let t = m.entry(run.id.clone()).or_default();
            t.seen = true;
            if command.is_none() && !is_shell_tool(tool) {
                return;
            }
            if id.is_some() && t.calls.iter().any(|c| c.id == id) {
                return;
            }
            t.calls.push(ToolCall {
                id,
                command,
                started_ms: now,
                ended_ms: None,
            });
            // Drop the oldest calls first, but keep those that own a counted process group.
            while t.calls.len() > TOOL_KEEP {
                let owns = |c: &ToolCall| {
                    c.id.is_some()
                        && run
                            .bg_tasks
                            .iter()
                            .any(|b| b.source == StateSource::Process && b.tool_use_id == c.id)
                };
                let Some(i) = t.calls.iter().position(|c| !owns(c)) else {
                    break;
                };
                t.calls.remove(i);
            }
        }
        "PostToolUse" | "PostToolUseFailure" => {
            let Some(id) = id else { return };
            let mut m = TOOLS.lock().unwrap();
            if let Some(c) = m.get_mut(&run.id).and_then(|t| {
                t.calls
                    .iter_mut()
                    .find(|c| c.id.as_deref() == Some(id.as_str()))
            }) {
                c.ended_ms.get_or_insert(now);
            }
        }
        _ => {}
    }
}

/// Harnesses that report their own background work (hooks, transcripts, protocols): the
/// process scan leaves them alone.
fn reports_own(harness: &str) -> bool {
    super::harness::Harness::from_id(harness).is_some_and(|h| {
        matches!(
            h.family(),
            super::harness::Family::Claude | super::harness::Family::Codex
        )
    })
}

/// Shells a harness runs tool commands with.
fn is_shell(argv: &[String]) -> bool {
    let a0 = argv.first().map(String::as_str).unwrap_or("");
    let base = a0.rsplit('/').next().unwrap_or(a0).trim_start_matches('-');
    matches!(base, "sh" | "bash" | "zsh" | "dash" | "fish" | "ksh")
}

/// The `-c` script of a shell command line.
fn script_of(argv: &[String]) -> Option<&str> {
    if !is_shell(argv) {
        return None;
    }
    let i = argv
        .iter()
        .position(|a| a.starts_with('-') && !a.starts_with("--") && a.ends_with('c'))?;
    argv.get(i + 1).map(String::as_str)
}

/// What a shell runs: the `-c` script, else the whole command line.
fn command_of(argv: &[String]) -> String {
    script_of(argv).map_or_else(|| argv.join(" "), str::to_string)
}

/// A command with quotes and backslashes removed and whitespace collapsed, so a harness that
/// quotes the command into a wrapper (`eval '…'`) still matches it.
fn norm_command(s: &str) -> String {
    let unquoted: String = s
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\\'))
        .collect();
    unquoted.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether a shell's `-c` script runs a tool call's command: one contains the other. The script
/// may hold only part of the command when it is at least half as long (a harness that strips a
/// prefix); a short script such as `bash` never matches a long command.
fn commands_match(script: &str, command: &str) -> bool {
    let (s, c) = (norm_command(script), norm_command(command));
    if s.is_empty() || c.is_empty() {
        return false;
    }
    s.contains(&c) || (c.contains(&s) && s.len() * 2 >= c.len())
}

/// What one process scan knows besides the process tree.
pub struct Scan<'a> {
    /// The harness process.
    pub root: u32,
    pub now: i64,
    /// When the harness process started (Unix ms), when the platform can tell.
    pub harness_started_ms: Option<i64>,
    /// The run's shell tool calls, or `None` when its harness reports no tool calls.
    pub calls: Option<&'a [ToolCall]>,
    /// The run is idle: a tool call without an end is not running any more.
    pub idle: bool,
}

/// The tool call that started a process group whose leader started at `start` and runs
/// `script`: it started at most [`SKEW_MS`] after the call started and before the call ended
/// (plus [`SKEW_MS`]), and runs the call's command. A call whose command is unknown matches on
/// time alone. The call must have ended, or the run be idle; before that the group is its
/// foreground work. The latest matching call wins, and a command match beats a time-only one.
fn claiming_call<'c>(
    calls: &'c [ToolCall],
    start: i64,
    script: Option<&str>,
    s: &Scan,
) -> Option<&'c ToolCall> {
    let in_time = |c: &&ToolCall| {
        start >= c.started_ms - SKEW_MS
            && start <= c.ended_ms.unwrap_or(s.now) + SKEW_MS
            && (c.ended_ms.is_some() || s.idle)
    };
    let by_command = calls
        .iter()
        .filter(in_time)
        .filter(|c| {
            c.command
                .as_deref()
                .zip(script)
                .is_some_and(|(cmd, sc)| commands_match(sc, cmd))
        })
        .max_by_key(|c| c.started_ms);
    by_command.or_else(|| {
        calls
            .iter()
            .filter(in_time)
            .filter(|c| c.command.is_none())
            .max_by_key(|c| c.started_ms)
    })
}

/// Background work of one harness process: process groups under it, other than its own, led by
/// a shell (how harnesses run tool commands; MCP servers and language servers are started
/// directly), alive for at least [`MIN_AGE_MS`]. One task per process group.
///
/// `started_ms` gives a process's start time (Unix ms): its real start when the platform can
/// tell, else when the scan first saw it. A group whose leader started before the harness
/// process is never its work.
///
/// When the harness reports tool calls (`calls` is `Some`), a group counts only when one of
/// them started it ([`claiming_call`]); the task carries that call's id and command. Without
/// tool calls every shell-led group counts, except those started within [`STARTUP_MS`] of the
/// harness process (its MCP servers and language servers behind `sh -c` wrappers).
pub fn scan_tree(
    s: &Scan,
    tree: &[vk_hold::procinfo::ProcInfo],
    started_ms: impl Fn(&vk_hold::procinfo::ProcInfo) -> Option<i64>,
) -> Vec<BgTask> {
    let own = tree.iter().find(|p| p.pid == s.root).map(|p| p.pgid);
    let mut out: Vec<BgTask> = Vec::new();
    for p in tree {
        let id = format!("pid:{}", p.pgid);
        if p.pid == s.root || Some(p.pgid) == own || out.iter().any(|t| t.id == id) {
            continue;
        }
        // The group's leader when it is in the tree, else the first member seen.
        let lead = tree.iter().find(|q| q.pid == p.pgid).unwrap_or(p);
        if !is_shell(&lead.argv) || vk_preview::is_harness_process(&lead.argv) {
            continue;
        }
        let Some(start) = started_ms(lead) else {
            continue;
        };
        if s.now - start < MIN_AGE_MS {
            continue;
        }
        if s.harness_started_ms.is_some_and(|t0| start < t0 - SKEW_MS) {
            continue;
        }
        let (tool_use_id, text) = match s.calls {
            Some(calls) => {
                let Some(c) = claiming_call(calls, start, script_of(&lead.argv), s) else {
                    continue;
                };
                (
                    c.id.clone(),
                    c.command.clone().unwrap_or_else(|| command_of(&lead.argv)),
                )
            }
            None => {
                if s.harness_started_ms
                    .is_some_and(|t0| start <= t0 + STARTUP_MS)
                {
                    continue;
                }
                (None, command_of(&lead.argv))
            }
        };
        out.push(BgTask {
            id,
            kind: "shell".into(),
            label: label(&text),
            since_ms: start,
            source: StateSource::Process,
            tool_use_id,
        });
    }
    out
}

/// Every [`SCAN_EVERY`]: child process groups of harnesses that report nothing themselves.
/// Without tool calls to match, a run's children may be foreground tool calls while it works,
/// so the set only shrinks then.
pub fn start(server: &Arc<Server>) {
    let srv = server.clone();
    tokio::spawn(async move {
        let mut seen: std::collections::HashMap<(u32, u64), i64> = Default::default();
        loop {
            tokio::time::sleep(SCAN_EVERY).await;
            let live: Vec<AgentRun> = srv.with_core(|c| {
                c.model
                    .runs
                    .iter()
                    .filter(|r| r.ended_at_ms.is_none() && !reports_own(&r.harness))
                    .cloned()
                    .collect::<Vec<_>>()
            });
            // Tool calls of ended or replaced runs are no longer needed.
            TOOLS
                .lock()
                .unwrap()
                .retain(|id, _| live.iter().any(|r| &r.id == id));
            let runs: Vec<(AgentRun, nested::Proc)> = live
                .into_iter()
                .filter_map(|r| {
                    let (h, p) = srv.agents.harness_proc(&r.pane)?;
                    (h == r.harness).then_some((r, p))
                })
                .collect();
            let roots: Vec<u32> = runs.iter().map(|(_, p)| p.pid).collect();
            let trees = tokio::task::spawn_blocking(move || {
                roots
                    .into_iter()
                    .map(|pid| vk_hold::procinfo::tree(pid, 8))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            let now = now_ms();
            // First sighting of each process (pid, start) stands in for its start time where
            // the platform cannot convert `start`.
            let alive: std::collections::HashSet<(u32, u64)> =
                trees.iter().flatten().map(|p| (p.pid, p.start)).collect();
            seen.retain(|k, _| alive.contains(k));
            for k in &alive {
                seen.entry(*k).or_insert(now);
            }
            for ((run, harness), tree) in runs.into_iter().zip(trees) {
                let started = |p: &vk_hold::procinfo::ProcInfo| {
                    vk_hold::procinfo::start_unix_ms(p.start)
                        .or_else(|| seen.get(&(p.pid, p.start)).copied())
                };
                let calls = tools_of(&run.id);
                let idle = run.execution.value == Execution::Idle;
                let scan = Scan {
                    root: harness.pid,
                    now,
                    harness_started_ms: vk_hold::procinfo::start_unix_ms(harness.start),
                    calls: calls.as_deref(),
                    idle,
                };
                let mut found = scan_tree(&scan, &tree, started);
                if calls.is_none() && !idle {
                    found.retain(|t| run.bg_tasks.iter().any(|o| o.id == t.id));
                }
                let key = |t: &BgTask| (t.id.clone(), t.tool_use_id.clone());
                let had: std::collections::HashSet<_> = run
                    .bg_tasks
                    .iter()
                    .filter(|t| t.source == StateSource::Process)
                    .map(key)
                    .collect();
                let now_set: std::collections::HashSet<_> = found.iter().map(key).collect();
                if had != now_set {
                    set_processes(&srv, &run.id, found);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_hook_entries_become_running_tasks() {
        let list = vec![
            json!({"id":"bdh72w7nj","type":"shell","status":"running","description":"build","command":"cargo build\n--release"}),
            json!({"id":"a1","type":"subagent","status":"running","description":"Map the code","agent_type":"Explore"}),
            json!({"id":"b2","type":"monitor","status":"running","description":"watch CI"}),
            json!({"id":"b3","type":"shell","status":"completed","command":"true"}),
            json!({"task_id":"t4","task_type":"local_workflow","description":"review"}),
        ];
        let t = from_claude(&list);
        assert_eq!(ids(&t), ["bdh72w7nj", "a1", "b2", "t4"]);
        assert_eq!(t[0].kind, "shell");
        assert_eq!(t[0].label, "cargo build");
        assert_eq!(t[1].kind, "subagent");
        assert_eq!(t[1].label, "Map the code");
        assert_eq!(t[2].kind, "monitor");
        assert_eq!(t[3].kind, "workflow");
    }

    fn proc(pid: u32, ppid: u32, pgid: u32, argv: &[&str]) -> vk_hold::procinfo::ProcInfo {
        vk_hold::procinfo::ProcInfo {
            pid,
            ppid,
            pgid,
            argv: argv.iter().map(|s| s.to_string()).collect(),
            exe: None,
            cwd: None,
            start: 0,
        }
    }

    /// Process start times (Unix ms) by pid, for `scan_tree`.
    fn starts(m: &[(u32, i64)]) -> impl Fn(&vk_hold::procinfo::ProcInfo) -> Option<i64> + '_ {
        move |p| m.iter().find(|(pid, _)| *pid == p.pid).map(|(_, t)| *t)
    }

    fn scan(now: i64, calls: Option<&[ToolCall]>, idle: bool) -> Scan<'_> {
        Scan {
            root: 100,
            now,
            harness_started_ms: Some(10_000),
            calls,
            idle,
        }
    }

    fn call(id: &str, command: &str, started_ms: i64, ended_ms: Option<i64>) -> ToolCall {
        ToolCall {
            id: Some(id.into()),
            command: Some(command.into()),
            started_ms,
            ended_ms,
        }
    }

    #[test]
    fn scan_without_tool_calls_counts_shell_groups_after_startup() {
        let tree = vec![
            proc(100, 1, 100, &["gemini"]),
            // An MCP server, started directly in the harness group.
            proc(101, 100, 100, &["node", "mcp-server.js"]),
            // A language server in its own group, not led by a shell.
            proc(102, 100, 102, &["rust-analyzer"]),
            // A background command and its child, one group.
            proc(103, 100, 103, &["/bin/bash", "-c", "npm run dev"]),
            proc(104, 103, 103, &["node", "vite"]),
            // Too young to count.
            proc(105, 100, 105, &["sh", "-c", "sleep 1"]),
            // An MCP server behind an `sh -c` wrapper, started with the harness.
            proc(106, 100, 106, &["sh", "-c", "npx some-mcp-server"]),
            // Older than the harness process: not its work.
            proc(107, 100, 107, &["sh", "-c", "tail -f log"]),
        ];
        let m = [
            (100, 10_000),
            (101, 10_500),
            (102, 10_500),
            (103, 30_000),
            (104, 30_100),
            (105, 37_500),
            (106, 11_000),
            (107, 2_000),
        ];
        let t = scan_tree(&scan(38_000, None, true), &tree, starts(&m));
        assert_eq!(ids(&t), ["pid:103"]);
        assert_eq!(t[0].label, "npm run dev");
        // The age is the process's real start, not the scan's first sighting.
        assert_eq!(t[0].since_ms, 30_000);
        assert_eq!(t[0].source, StateSource::Process);
        assert_eq!(t[0].tool_use_id, None);
    }

    #[test]
    fn scan_with_tool_calls_counts_only_groups_a_tool_call_started() {
        let tree = vec![
            proc(100, 1, 100, &["pi"]),
            // An MCP server behind `sh -c`, started at harness startup.
            proc(101, 100, 101, &["sh", "-c", "npx some-mcp-server"]),
            // The user's `!cmd` shell while the agent was idle: no tool call started it.
            proc(102, 100, 102, &["/bin/bash", "-c", "python -m http.server"]),
            // `npm run dev &` from a bash tool call, alive after the call ended.
            proc(103, 100, 103, &["/bin/bash", "-c", "npm  run dev &"]),
            proc(104, 103, 103, &["node", "vite"]),
        ];
        let m = [
            (100, 10_000),
            (101, 10_800),
            (102, 50_000),
            (103, 30_200),
            (104, 30_300),
        ];
        let calls = [
            call("call_ls", "ls", 20_000, Some(20_100)),
            call("call_dev", "npm run dev &", 30_000, Some(31_000)),
        ];
        let t = scan_tree(&scan(60_000, Some(&calls), true), &tree, starts(&m));
        assert_eq!(ids(&t), ["pid:103"]);
        assert_eq!(t[0].tool_use_id.as_deref(), Some("call_dev"));
        assert_eq!(t[0].label, "npm run dev &");
        assert_eq!(t[0].since_ms, 30_200);
    }

    #[test]
    fn scan_counts_a_tool_started_group_while_working_once_its_call_ended() {
        let tree = vec![
            proc(100, 1, 100, &["omp"]),
            proc(103, 100, 103, &["bash", "-c", "cargo watch -x test"]),
        ];
        let m = [(100, 10_000), (103, 30_200)];
        let running = [call("c1", "cargo watch -x test", 30_000, None)];
        // The call still runs: the group is its foreground work.
        let t = scan_tree(&scan(40_000, Some(&running), false), &tree, starts(&m));
        assert!(t.is_empty());
        // Idle: a call without an end is not running any more.
        let t = scan_tree(&scan(40_000, Some(&running), true), &tree, starts(&m));
        assert_eq!(ids(&t), ["pid:103"]);
        // The call ended and the group lives on: counted while the run works.
        let ended = [call("c1", "cargo watch -x test", 30_000, Some(31_000))];
        let t = scan_tree(&scan(40_000, Some(&ended), false), &tree, starts(&m));
        assert_eq!(t[0].tool_use_id.as_deref(), Some("c1"));
        // A group started outside the call's time does not match it.
        let late = [(100, 10_000), (103, 36_000)];
        let t = scan_tree(&scan(40_000, Some(&ended), false), &tree, starts(&late));
        assert!(t.is_empty());
        // Nor does one that runs another command.
        let other = [call("c1", "npm test", 30_000, Some(31_000))];
        let t = scan_tree(&scan(40_000, Some(&other), false), &tree, starts(&m));
        assert!(t.is_empty());
    }

    #[test]
    fn commands_match_through_wrappers() {
        assert!(commands_match(
            "source ~/.snapshot && eval 'npm run dev &'",
            "npm run dev &"
        ));
        assert!(commands_match("echo  \"a b\"", "echo a b"));
        assert!(!commands_match(
            "bash",
            "bash ./scripts/long-running-job.sh"
        ));
        assert!(!commands_match("", "ls"));
    }

    #[test]
    fn labels_are_redacted_and_shortened() {
        let l = label(&format!(
            "curl -H 'Authorization: Bearer sk-ant-api03-{}' x",
            "a".repeat(90)
        ));
        assert!(!l.contains("aaaaaaaaaaaaaaaaaaaa"), "{l}");
        let long = label(&"x".repeat(400));
        assert_eq!(long.chars().count(), LABEL_MAX + 1);
    }
}
