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
        Some((next, now))
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
        "Stop" => {
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
