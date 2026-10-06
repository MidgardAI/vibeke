//! Gemini CLI hooks (04 §6.5) → the hook vocabulary.
//!
//! [verify M2] Hook names (`SessionStart`, `BeforeAgent`, `AfterAgent`, `BeforeTool`,
//! `AfterTool`, `Notification`, `PreCompress`, `SessionEnd`) and payload fields (`session_id`,
//! `transcript_path`, `cwd`, `prompt`, `prompt_response`, `tool_name`, `tool_input`,
//! `tool_response`, `notification_type`, `details`) follow the upstream hooks docs; none has been
//! exercised against a live binary. Whether a hook can answer the native tool confirmation is
//! unverified, so confirmations open observe-only approvals answered by best-effort keystrokes.
//! Gemini payloads carry no tool-call id: ids are synthesized per pane (FIFO by tool name).

use super::harness::Harness;
use super::*;
use std::collections::VecDeque;
use std::sync::LazyLock;

/// pane → pending `(tool name, synthesized call id)`.
type Pending = HashMap<String, VecDeque<(String, String)>>;

static PENDING: LazyLock<Mutex<Pending>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn tool_name(t: &str) -> String {
    match t {
        "run_shell_command" => "Bash".into(),
        "write_file" => "Write".into(),
        "replace" | "edit" => "Edit".into(),
        "read_file" | "read_many_files" => "Read".into(),
        "glob" => "Glob".into(),
        "search_file_content" | "grep" => "Grep".into(),
        "web_fetch" => "WebFetch".into(),
        other => other.to_string(),
    }
}

pub fn normalize_input(input: &Value) -> Value {
    let mut v = input.clone();
    if let Some(o) = v.as_object_mut()
        && !o.contains_key("file_path")
        && let Some(p) = o.get("absolute_path").or_else(|| o.get("path")).cloned()
    {
        o.insert("file_path".into(), p);
    }
    v
}

fn push_call(pane: &str, tool: &str) -> String {
    let id = format!("gemini-{}", ulid());
    PENDING
        .lock()
        .unwrap()
        .entry(pane.to_string())
        .or_default()
        .push_back((tool.to_string(), id.clone()));
    id
}

fn pop_call(pane: &str, tool: &str) -> Option<String> {
    let mut g = PENDING.lock().unwrap();
    let q = g.get_mut(pane)?;
    let i = q.iter().position(|(t, _)| t == tool)?;
    q.remove(i).map(|(_, id)| id)
}

fn latest_call(pane: &str) -> Option<String> {
    PENDING
        .lock()
        .unwrap()
        .get(pane)
        .and_then(|q| q.back().map(|(_, id)| id.clone()))
}

/// Translate one Gemini hook into hook-vocabulary calls (returned for tests).
pub(super) fn translate(pane: &str, event: &str, p: &Value) -> Vec<(&'static str, Value)> {
    let sid = p.get("session_id").cloned().unwrap_or(Value::Null);
    match event {
        "SessionStart" => vec![("SessionStart", p.clone())],
        "BeforeAgent" => vec![(
            "UserPromptSubmit",
            json!({"prompt": p.get("prompt"), "session_id": sid}),
        )],
        "AfterAgent" => {
            PENDING.lock().unwrap().remove(pane);
            vec![(
                "Stop",
                json!({"last_assistant_message": p.get("prompt_response"), "session_id": sid, "transcript_path": p.get("transcript_path")}),
            )]
        }
        "BeforeTool" => {
            let raw = p.get("tool_name").and_then(Value::as_str).unwrap_or("");
            let id = push_call(pane, raw);
            vec![(
                "PreToolUse",
                json!({"tool_name": tool_name(raw), "tool_input": normalize_input(p.get("tool_input").unwrap_or(&Value::Null)), "tool_use_id": id, "session_id": sid}),
            )]
        }
        "AfterTool" => {
            let raw = p.get("tool_name").and_then(Value::as_str).unwrap_or("");
            let id = pop_call(pane, raw);
            let resp = p.get("tool_response").cloned().unwrap_or(Value::Null);
            let failed = resp.get("error").is_some_and(|e| !e.is_null());
            let exit = resp
                .get("exitCode")
                .or_else(|| resp.get("exit_code"))
                .cloned();
            vec![(
                if failed {
                    "PostToolUseFailure"
                } else {
                    "PostToolUse"
                },
                json!({"tool_name": tool_name(raw), "tool_use_id": id, "tool_input": normalize_input(p.get("tool_input").unwrap_or(&Value::Null)), "exit_code": exit}),
            )]
        }
        "Notification" => {
            let ty = p
                .get("notification_type")
                .and_then(Value::as_str)
                .unwrap_or("");
            if ty == "ToolPermission" {
                let details = p.get("details").cloned().unwrap_or(Value::Null);
                vec![(
                    "Confirmation",
                    json!({"tool_use_id": latest_call(pane), "details": details, "message": p.get("message")}),
                )]
            } else {
                vec![("Notification", p.clone())]
            }
        }
        "PreCompress" => vec![("PreCompact", json!({}))],
        "SessionEnd" => {
            PENDING.lock().unwrap().remove(pane);
            vec![("SessionEnd", p.clone())]
        }
        "AfterModel" => vec![("Usage", p.clone())],
        _ => vec![],
    }
}

pub(super) fn on_hook(server: &Arc<Server>, pane: &str, h: Harness, event: &str, p: &Value) {
    for (ev, payload) in translate(pane, event, p) {
        match ev {
            "Confirmation" => {
                let run = bound_run(server, pane, h);
                let d = &payload["details"];
                let tool = d
                    .get("tool_name")
                    .or_else(|| d.get("toolName"))
                    .and_then(Value::as_str)
                    .map(tool_name)
                    .unwrap_or_else(|| "Bash".into());
                let mut input = normalize_input(d);
                if input.get("command").is_none()
                    && let (Some(o), Some(m)) = (input.as_object_mut(), payload["message"].as_str())
                {
                    o.insert("command".into(), json!(m));
                }
                let input = if input.is_object() {
                    input
                } else {
                    json!({"command": payload["message"]})
                };
                super::route::open_observed(
                    server,
                    &run,
                    payload["tool_use_id"].as_str().map(str::to_string),
                    &tool,
                    input,
                    StateSource::Structured,
                    1.0,
                    true,
                );
            }
            "Usage" => {
                let run = bound_run(server, pane, h);
                super::usage::from_gemini(server, &run, &payload);
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
        let pane = "gemini-test-pane";
        let pre = translate(
            pane,
            "BeforeTool",
            &json!({"session_id": "g1", "tool_name": "run_shell_command", "tool_input": {"command": "cargo test"}}),
        );
        assert_eq!(pre[0].0, "PreToolUse");
        assert_eq!(pre[0].1["tool_name"], "Bash");
        let id = pre[0].1["tool_use_id"].as_str().unwrap().to_string();
        let conf = translate(
            pane,
            "Notification",
            &json!({"notification_type": "ToolPermission", "message": "Allow execution of: 'cargo'?", "details": {"command": "cargo test"}}),
        );
        assert_eq!(conf[0].0, "Confirmation");
        assert_eq!(conf[0].1["tool_use_id"], id.as_str());
        let post = translate(
            pane,
            "AfterTool",
            &json!({"tool_name": "run_shell_command", "tool_input": {"command": "cargo test"}, "tool_response": {"exitCode": 0}}),
        );
        assert_eq!(post[0].0, "PostToolUse");
        assert_eq!(post[0].1["tool_use_id"], id.as_str());
        assert_eq!(post[0].1["exit_code"], 0);
        let stop = translate(
            pane,
            "AfterAgent",
            &json!({"prompt": "x", "prompt_response": "All green."}),
        );
        assert_eq!(stop[0].0, "Stop");
        assert_eq!(stop[0].1["last_assistant_message"], "All green.");
        let rd = translate(
            pane,
            "BeforeTool",
            &json!({"tool_name": "read_file", "tool_input": {"absolute_path": "/w/a"}}),
        );
        assert_eq!(rd[0].1["tool_input"]["file_path"], "/w/a");
        assert_eq!(
            translate(pane, "BeforeAgent", &json!({"prompt": "go"}))[0].0,
            "UserPromptSubmit"
        );
    }
}
