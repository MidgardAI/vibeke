//! Hermes Agent plugin hooks (04 §5.3) → the hook vocabulary. The plugin
//! (`integrations/hermes-plugin/__init__.py`) forwards Hermes plugin hooks through
//! `vibeke hook hermes <hook>` with their keyword arguments; this module owns the mapping.
//!
//! Hermes plugin approval hooks are observers: they cannot answer the dialog. A prompted
//! approval opens an interaction answered by keystrokes in the pane, and closes when Hermes
//! reports the decision. Clarify questions and sudo prompts open interactions that are answered
//! in the pane. The plugin drops subagent sessions, so their turns never settle the
//! parent's run.

use super::harness::Harness;
use super::*;
use std::sync::LazyLock;

/// pane → the last final answer (`post_llm_call` comes before `on_session_end`).
static LAST_TEXT: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn tool_name(t: &str) -> String {
    match t {
        "terminal" | "execute_code" => "Bash".into(),
        "write_file" => "Write".into(),
        "patch" => "Edit".into(),
        "read_file" => "Read".into(),
        "search_files" => "Grep".into(),
        "web_extract" => "WebFetch".into(),
        "web_search" => "WebSearch".into(),
        "delegate_task" => "Task".into(),
        "todo" => "TodoWrite".into(),
        other => other.to_string(),
    }
}

/// Hermes tool args → the field names the risk and summary code reads.
pub fn normalize_args(args: &Value) -> Value {
    let mut v = args.clone();
    if let Some(o) = v.as_object_mut() {
        if !o.contains_key("file_path")
            && let Some(p) = o.get("path").cloned()
        {
            o.insert("file_path".into(), p);
        }
        if !o.contains_key("command")
            && let Some(c) = o.get("code").cloned()
        {
            o.insert("command".into(), c);
        }
    }
    v
}

/// Translate one Hermes hook into hook-vocabulary calls (returned for tests).
pub(super) fn translate(pane: &str, event: &str, p: &Value) -> Vec<(&'static str, Value)> {
    let sid = p.get("session_id").cloned().unwrap_or(Value::Null);
    match event {
        "on_session_start" => vec![(
            "SessionStart",
            json!({"session_id": sid, "source": "startup", "model": p.get("model")}),
        )],
        // `/new` or `/clear`: the pane continues in a fresh session.
        "on_session_reset" => vec![(
            "SessionStart",
            json!({"session_id": sid, "source": "clear"}),
        )],
        "pre_llm_call" => vec![(
            "UserPromptSubmit",
            json!({"prompt": p.get("prompt"), "session_id": sid}),
        )],
        "post_llm_call" => {
            if let Some(t) = p.get("assistant_response").and_then(Value::as_str) {
                LAST_TEXT
                    .lock()
                    .unwrap()
                    .insert(pane.to_string(), t.to_string());
            }
            vec![]
        }
        // Fires at the end of every turn, whatever its outcome.
        "on_session_end" => {
            let text = LAST_TEXT.lock().unwrap().remove(pane);
            let failed = p.get("failed").and_then(Value::as_bool) == Some(true)
                && p.get("interrupted").and_then(Value::as_bool) != Some(true);
            if failed {
                let reason = p
                    .get("turn_exit_reason")
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                vec![("StopFailure", json!({"error_type": reason}))]
            } else {
                vec![(
                    "Stop",
                    json!({"last_assistant_message": text, "session_id": sid}),
                )]
            }
        }
        "on_session_finalize" => vec![(
            "SessionEnd",
            json!({"session_id": sid, "reason": p.get("reason")}),
        )],
        "pre_tool_call" => {
            let raw = p.get("tool_name").and_then(Value::as_str).unwrap_or("");
            vec![(
                "PreToolUse",
                json!({"tool_name": tool_name(raw), "tool_input": normalize_args(p.get("args").unwrap_or(&Value::Null)), "tool_use_id": p.get("tool_call_id"), "session_id": sid}),
            )]
        }
        "post_tool_call" => {
            let raw = p.get("tool_name").and_then(Value::as_str).unwrap_or("");
            let failed = p.get("error_type").is_some_and(|e| !e.is_null())
                || p.get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|s| !matches!(s, "success" | "ok" | "completed"));
            vec![(
                if failed {
                    "PostToolUseFailure"
                } else {
                    "PostToolUse"
                },
                json!({"tool_name": tool_name(raw), "tool_use_id": p.get("tool_call_id"), "tool_input": normalize_args(p.get("args").unwrap_or(&Value::Null)), "session_id": sid}),
            )]
        }
        "pre_approval_request" => vec![("Approval", p.clone())],
        // The decision (or a timeout) closes the approval, whoever made it.
        "post_approval_response" => match p.get("tool_call_id").and_then(Value::as_str) {
            Some(id) => vec![("PermissionDenied", json!({"tool_use_id": id}))],
            None => vec![("ApprovalClosed", json!({}))],
        },
        // A clarify question or a sudo password prompt: answered in the pane. Approvals come
        // through `pre_approval_request`.
        "on_human_input_request" => match p.get("kind").and_then(Value::as_str) {
            Some("clarify" | "sudo") => vec![("Input", p.clone())],
            _ => vec![],
        },
        "on_human_input_resolved" => match p.get("request_id").and_then(Value::as_str) {
            Some(id) => vec![("PermissionDenied", json!({"tool_use_id": id}))],
            None => vec![],
        },
        "subagent_start" => vec![(
            "SubagentStart",
            json!({"agent_id": p.get("child_session_id"), "agent_type": p.get("child_role")}),
        )],
        "subagent_stop" => vec![(
            "SubagentStop",
            json!({"agent_id": p.get("child_session_id")}),
        )],
        "post_api_request" => vec![("Usage", p.clone())],
        _ => vec![],
    }
}

pub(super) fn on_hook(server: &Arc<Server>, pane: &str, h: Harness, event: &str, p: &Value) {
    for (ev, payload) in translate(pane, event, p) {
        match ev {
            "Approval" => {
                let run = bound_run(server, pane, h);
                let mut input = json!({"command": payload.get("command")});
                if let Some(d) = payload.get("description") {
                    input["description"] = d.clone();
                }
                super::route::open_observed(
                    server,
                    &run,
                    payload
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    "Bash",
                    input,
                    StateSource::Structured,
                    1.0,
                    true,
                );
            }
            "Input" => {
                let run = bound_run(server, pane, h);
                let prompt = match payload.get("kind").and_then(Value::as_str) {
                    Some("sudo") => "Hermes needs your sudo password".to_string(),
                    _ => payload
                        .get("prompt")
                        .and_then(Value::as_str)
                        .unwrap_or("Hermes needs your input")
                        .to_string(),
                };
                super::route::open_observed(
                    server,
                    &run,
                    payload
                        .get("request_id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    "AskUserQuestion",
                    json!({"questions": [{"question": prompt}]}),
                    StateSource::Structured,
                    1.0,
                    false,
                );
            }
            "ApprovalClosed" => {
                let run = bound_run(server, pane, h);
                for id in super::route::open_of(server, &run.id, Some(StateSource::Structured)) {
                    resolve(
                        server,
                        &id,
                        InteractionStatus::ResolvedElsewhere,
                        "approval answered",
                    );
                }
            }
            "Usage" => {
                let run = bound_run(server, pane, h);
                super::usage::from_hermes(server, &run, &payload);
            }
            other => on_signal(server, pane, h, other, &payload),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_translate_to_the_shared_vocabulary() {
        let pane = "hermes-test-pane";
        let events = |e: &str, p: Value| {
            translate(pane, e, &p)
                .into_iter()
                .map(|(e, _)| e)
                .collect::<Vec<_>>()
        };
        let start = translate(
            pane,
            "on_session_start",
            &json!({"session_id": "20261012_1", "platform": "cli"}),
        );
        assert_eq!(start[0].0, "SessionStart");
        assert_eq!(start[0].1["session_id"], "20261012_1");
        assert_eq!(
            events(
                "pre_llm_call",
                json!({"session_id": "20261012_1", "prompt": "fix it"})
            ),
            vec!["UserPromptSubmit"]
        );
        let pre = translate(
            pane,
            "pre_tool_call",
            &json!({"tool_name": "patch", "tool_call_id": "call_1", "args": {"path": "/w/a.py", "old_string": "a", "new_string": "b"}}),
        );
        assert_eq!(pre[0].0, "PreToolUse");
        assert_eq!(pre[0].1["tool_name"], "Edit");
        assert_eq!(pre[0].1["tool_input"]["file_path"], "/w/a.py");
        assert_eq!(pre[0].1["tool_use_id"], "call_1");
        let post = translate(
            pane,
            "post_tool_call",
            &json!({"tool_name": "terminal", "tool_call_id": "call_2", "args": {"command": "false"}, "status": "error", "error_type": "exit_code"}),
        );
        assert_eq!(post[0].0, "PostToolUseFailure");
        assert_eq!(post[0].1["tool_name"], "Bash");
        assert_eq!(
            events(
                "post_approval_response",
                json!({"tool_call_id": "call_2", "choice": "once"})
            ),
            vec!["PermissionDenied"]
        );
        assert!(events("post_llm_call", json!({"assistant_response": "All green."})).is_empty());
        let stop = translate(
            pane,
            "on_session_end",
            &json!({"session_id": "20261012_1", "completed": true}),
        );
        assert_eq!(stop[0].0, "Stop");
        assert_eq!(stop[0].1["last_assistant_message"], "All green.");
        assert_eq!(
            events(
                "on_session_end",
                json!({"failed": true, "turn_exit_reason": "api_error"})
            ),
            vec!["StopFailure"]
        );
        assert_eq!(
            events(
                "on_session_end",
                json!({"failed": true, "interrupted": true})
            ),
            vec!["Stop"]
        );
    }
}
