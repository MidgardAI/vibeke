//! Background work of an agent run (`AgentRun::bg_tasks`): shells, monitors and subagents the
//! harness keeps running after its turn ended.
//!
//! Three kinds of input, best first:
//! - a whole set the harness reports ([`replace`]): Claude `Stop.background_tasks`, Claude
//!   stream-json `background_tasks_changed`, Codex `thread/backgroundTerminals/list`;
//! - start and end records from the transcript ([`started`], [`ended`]);
//! - child processes of the harness that outlive its turn ([`set_processes`]), for harnesses
//!   with neither.
//!
//! A reported set wins over transcript records it already covers: a start older than
//! `bg_level_ms` is history (the tailer replays whole files after a restart). Ends always apply.

use std::sync::Arc;

use serde_json::{Value, json};
use vk_proto::model::{AgentRun, BgTask, Execution, StateSource};

use super::update_run;
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
        next.extend(
            r.bg_tasks
                .iter()
                .filter(|t| t.source == StateSource::Process)
                .cloned(),
        );
        Some((next, now - SKEW_MS))
    });
}

/// A transcript record says `task` started at `task.since_ms`.
pub fn started(server: &Arc<Server>, run: &str, task: BgTask) {
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
            if let Some(old) = r.bg_tasks.iter().find(|o| o.id == p.id) {
                p.since_ms = old.since_ms;
            }
            next.push(p);
        }
        Some((next, r.bg_level_ms))
    });
}

/// Forget everything: the harness session started over or its process exited (its children
/// went with it, or are no longer its work).
pub fn clear(server: &Arc<Server>, run: &str) {
    let now = now_ms();
    store(server, run, |_| Some((vec![], now)));
}

/// Hook payloads: Claude `Stop` carries the session's whole background set.
pub fn observe_hook(server: &Arc<Server>, run: &AgentRun, event: &str, p: &Value) {
    match event {
        // Claude `Stop`, and `BgTasks` from headless adapters (Claude stream-json
        // `background_tasks_changed`, Codex `thread/backgroundTerminals/list`).
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

/// What a shell runs: the `-c` script, else the whole command line.
fn command_of(argv: &[String]) -> String {
    if is_shell(argv)
        && let Some(i) = argv
            .iter()
            .position(|a| a.starts_with('-') && a.ends_with('c'))
        && let Some(script) = argv.get(i + 1)
    {
        return script.clone();
    }
    argv.join(" ")
}

/// Background work of one harness process: process groups under it, other than its own, led by
/// a shell (how harnesses run tool commands; MCP servers and language servers are started
/// directly), alive for at least [`MIN_AGE_MS`]. One task per process group.
pub fn scan_tree(
    root: u32,
    tree: &[vk_hold::procinfo::ProcInfo],
    now: i64,
    age_ms: impl Fn(u32) -> Option<i64>,
) -> Vec<BgTask> {
    let own = tree.iter().find(|p| p.pid == root).map(|p| p.pgid);
    let mut out: Vec<BgTask> = Vec::new();
    for p in tree {
        if p.pid == root
            || Some(p.pgid) == own
            || out.iter().any(|t| t.id == format!("pid:{}", p.pgid))
        {
            continue;
        }
        // The group's leader when it is in the tree, else the first member seen.
        let lead = tree.iter().find(|q| q.pid == p.pgid).unwrap_or(p);
        if !is_shell(&lead.argv) || vk_preview::is_harness_process(&lead.argv) {
            continue;
        }
        if age_ms(lead.pid).is_none_or(|a| a < MIN_AGE_MS) {
            continue;
        }
        out.push(BgTask {
            id: format!("pid:{}", p.pgid),
            kind: "shell".into(),
            label: label(&command_of(&lead.argv)),
            since_ms: now - age_ms(lead.pid).unwrap_or(0),
            source: StateSource::Process,
            tool_use_id: None,
        });
    }
    out
}

/// Every [`SCAN_EVERY`]: child process groups of idle harnesses that report nothing themselves.
/// While a run works its children may be foreground tool calls, so the set only shrinks then.
pub fn start(server: &Arc<Server>) {
    let srv = server.clone();
    tokio::spawn(async move {
        let mut seen: std::collections::HashMap<(u32, u64), i64> = Default::default();
        loop {
            tokio::time::sleep(SCAN_EVERY).await;
            let runs: Vec<(AgentRun, u32)> = srv
                .with_core(|c| {
                    c.model
                        .runs
                        .iter()
                        .filter(|r| r.ended_at_ms.is_none() && !reports_own(&r.harness))
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .into_iter()
                .filter_map(|r| {
                    let (h, p) = srv.agents.harness_proc(&r.pane)?;
                    (h == r.harness).then_some((r, p.pid))
                })
                .collect();
            let roots: Vec<u32> = runs.iter().map(|(_, pid)| *pid).collect();
            let trees = tokio::task::spawn_blocking(move || {
                roots
                    .into_iter()
                    .map(|pid| vk_hold::procinfo::tree(pid, 8))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            let now = now_ms();
            // First sighting of each process (pid, start) stands in for its age.
            let alive: std::collections::HashSet<(u32, u64)> =
                trees.iter().flatten().map(|p| (p.pid, p.start)).collect();
            seen.retain(|k, _| alive.contains(k));
            for k in &alive {
                seen.entry(*k).or_insert(now);
            }
            for ((run, root), tree) in runs.into_iter().zip(trees) {
                let age = |pid: u32| {
                    tree.iter()
                        .find(|p| p.pid == pid)
                        .and_then(|p| seen.get(&(p.pid, p.start)))
                        .map(|t| now - t)
                };
                let mut found = scan_tree(root, &tree, now, age);
                if run.execution.value != Execution::Idle {
                    found.retain(|t| run.bg_tasks.iter().any(|o| o.id == t.id));
                }
                let had: Vec<&BgTask> = run
                    .bg_tasks
                    .iter()
                    .filter(|t| t.source == StateSource::Process)
                    .collect();
                if had.len() != found.len() || had.iter().zip(&found).any(|(a, b)| a.id != b.id) {
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

    #[test]
    fn scan_counts_shell_groups_outside_the_harness_group() {
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
        ];
        let t = scan_tree(100, &tree, 10_000, |pid| {
            Some(if pid == 105 { 500 } else { 8_000 })
        });
        assert_eq!(ids(&t), ["pid:103"]);
        assert_eq!(t[0].label, "npm run dev");
        assert_eq!(t[0].since_ms, 2_000);
        assert_eq!(t[0].source, StateSource::Process);
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
