//! Codex runs sub-agents (multi-agent `spawn_agent`) and internal helpers (memory
//! consolidation) as threads of the pane's own Codex process. Their hooks reach the server with
//! the pane's process chain, so [`super::nested`] cannot tell them from the pane's thread.
//! Seen in practice: a memory consolidation thread's `SessionStart` (its own session id, no
//! transcript) rebound the pane's run to that thread, set it idle mid-turn and cleared its
//! background work; its `SessionEnd` then ended the run's session.
//!
//! A Codex hook comes from another thread of the process when:
//! - its `agent_id`, or a `session_id` other than the run's, is a known sub-agent: named by a
//!   `SubagentStart` hook (`agent_id` is the child thread id) or by a background task of the
//!   run (the transcript reports sub-agents under their thread id);
//! - its `session_id` is not the run's and its `transcript_path` is null (an internal thread
//!   that keeps no rollout);
//! - its `session_id` is not the run's and its rollout's `session_meta` says it is a sub-agent
//!   (`thread_source: subagent`, or this run's session as `parent_thread_id`).
//!
//! `SubagentStart` and `SubagentStop` are the parent's own hooks and always go through.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::{LazyLock, Mutex};

use serde_json::Value;

use crate::Server;

/// Known sub-agent threads remembered per pane.
const KNOWN_MAX: usize = 256;
/// How much of a rollout's head is read for its `session_meta` (the fields that name a parent
/// come before the long instructions).
const HEAD_BYTES: u64 = 64 * 1024;

/// Sub-agent thread ids per pane, from `SubagentStart` hooks.
static KNOWN: LazyLock<Mutex<HashMap<String, HashSet<String>>>> = LazyLock::new(Default::default);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Thread {
    /// The pane's own thread (or nothing says otherwise).
    Own,
    /// A sub-agent of the pane's thread.
    Child,
    /// A thread without a rollout (memory consolidation and the like).
    Internal,
}

fn set<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str).filter(|v| !v.is_empty())
}

fn note(pane: &str, id: &str) {
    let mut m = KNOWN.lock().unwrap();
    let ids = m.entry(pane.to_string()).or_default();
    if ids.len() >= KNOWN_MAX {
        ids.clear();
    }
    ids.insert(id.to_string());
}

fn noted(pane: &str, id: &str) -> bool {
    KNOWN
        .lock()
        .unwrap()
        .get(pane)
        .is_some_and(|ids| ids.contains(id))
}

/// Whether a rollout's head names a parent thread: it belongs to a sub-agent.
pub(super) fn head_is_subagent(head: &str, parent: &str) -> bool {
    let compact: String = head.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains(r#""thread_source":"subagent""#)
        || compact.contains(&format!(r#""parent_thread_id":"{parent}""#))
}

fn rollout_is_subagent(path: &str, parent: &str) -> bool {
    let Ok(f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = Vec::new();
    if f.take(HEAD_BYTES).read_to_end(&mut buf).is_err() {
        return false;
    }
    let head = String::from_utf8_lossy(&buf);
    let first = head.split('\n').next().unwrap_or("");
    head_is_subagent(first, parent)
}

/// Which thread of the pane's Codex process a hook comes from.
pub(super) fn classify(server: &Server, pane: &str, event: &str, p: &Value) -> Thread {
    if matches!(event, "SubagentStart" | "SubagentStop") {
        if event == "SubagentStart"
            && let Some(id) = set(p, "agent_id")
        {
            note(pane, id);
        }
        return Thread::Own;
    }
    let Some(run) = server.with_core(|c| c.run_for_pane(pane).cloned()) else {
        return Thread::Own;
    };
    let known = |id: &str| {
        noted(pane, id)
            || run.bg_tasks.iter().any(|t| {
                t.kind == "subagent"
                    && (t.id == id
                        || t.id
                            .strip_prefix(id)
                            .is_some_and(|rest| rest.starts_with('#')))
            })
    };
    if set(p, "agent_id").is_some_and(known) {
        return Thread::Child;
    }
    let (Some(sid), Some(own)) = (set(p, "session_id"), run.harness_session_id.as_deref()) else {
        return Thread::Own;
    };
    if sid == own {
        return Thread::Own;
    }
    if known(sid) {
        return Thread::Child;
    }
    match p.get("transcript_path") {
        Some(Value::Null) => Thread::Internal,
        Some(Value::String(path)) if rollout_is_subagent(path, own) => {
            note(pane, sid);
            Thread::Child
        }
        _ => Thread::Own,
    }
}

/// What of a sub-agent's hook reaches the pane's run: its tool results (items, file changes,
/// resolving an approval its gate opened), without the session id and transcript path that
/// would rebind the run. Everything else (session, prompt and stop hooks) is dropped.
pub(super) fn child_signal(event: &str, p: &Value) -> Option<Value> {
    if !matches!(
        event,
        "PostToolUse" | "PostToolUseFailure" | "PermissionDenied"
    ) {
        return None;
    }
    let mut p = p.clone();
    if let Some(m) = p.as_object_mut() {
        m.remove("session_id");
        m.remove("transcript_path");
        m.remove("permission_mode");
    }
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rollout_heads_of_subagents() {
        let child = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"session_id":"c1","id":"c1","source":{"subagent":{"thread_spawn":{"parent_thread_id":"p1","depth":1,"agent_path":"/root/worker"}}},"thread_source":"subagent","parent_thread_id":"p1","agent_path":"/root/worker"}}"#;
        let root = r#"{"timestamp":"2026-01-01T00:00:00Z","type":"session_meta","payload":{"session_id":"p1","id":"p1","source":"cli","thread_source":"user","parent_thread_id":null}}"#;
        assert!(head_is_subagent(child, "p1"));
        assert!(head_is_subagent(child, "other"), "thread_source alone");
        assert!(!head_is_subagent(root, "p1"));
        let spaced = r#"{"payload": {"parent_thread_id": "p1"}}"#;
        assert!(head_is_subagent(spaced, "p1"));
    }

    #[test]
    fn child_signals_keep_tool_results_without_the_session() {
        let p = json!({"session_id": "c1", "transcript_path": "/x.jsonl", "tool_name": "Bash", "tool_use_id": "t1", "agent_id": "c1"});
        let out = child_signal("PostToolUse", &p).unwrap();
        assert!(out.get("session_id").is_none());
        assert!(out.get("transcript_path").is_none());
        assert_eq!(out["tool_use_id"], "t1");
        for e in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "Stop",
            "Interrupt",
            "SessionEnd",
            "PreCompact",
        ] {
            assert!(child_signal(e, &p).is_none(), "{e}");
        }
    }
}
