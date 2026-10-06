//! OpenCode plugin events (04 §6.4) → the hook vocabulary. The plugin
//! (`integrations/opencode-plugin/vibeke.ts`) forwards raw OpenCode events through
//! `vibeke hook opencode <event>`; this module owns the mapping so it is testable in Rust.
//!
//! [verify M2] Event names and payload shapes follow the upstream plugin docs and have not been
//! exercised against a live OpenCode. Subagents: child sessions (`parentID`) keep
//! the parent `working` until every child is idle.

use super::harness::Harness;
use super::*;
use std::collections::HashSet;
use std::sync::LazyLock;

#[derive(Default)]
struct PaneState {
    main: Option<String>,
    busy_children: HashSet<String>,
    children: HashSet<String>,
    /// The main session went idle while children were still busy.
    idle_pending: bool,
    last_text: Option<String>,
}

static STATE: LazyLock<Mutex<HashMap<String, PaneState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn tool_name(t: &str) -> String {
    match t {
        "bash" => "Bash".into(),
        "edit" | "patch" | "multiedit" => "Edit".into(),
        "write" => "Write".into(),
        "read" => "Read".into(),
        "glob" => "Glob".into(),
        "grep" => "Grep".into(),
        "webfetch" => "WebFetch".into(),
        "task" => "Task".into(),
        other => other.to_string(),
    }
}

/// OpenCode's camelCase tool args → the field names the risk/summary code reads.
pub fn normalize_args(args: &Value) -> Value {
    let Some(o) = args.as_object() else {
        return args.clone();
    };
    let mut out = serde_json::Map::new();
    for (k, v) in o {
        let k = match k.as_str() {
            "filePath" => "file_path",
            "oldString" => "old_string",
            "newString" => "new_string",
            other => other,
        };
        out.insert(k.to_string(), v.clone());
    }
    Value::Object(out)
}

fn sid(p: &Value) -> Option<String> {
    p.get("sessionID")
        .or_else(|| p.pointer("/info/id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Translate one OpenCode event into hook-vocabulary calls (returned for tests).
pub(super) fn translate(pane: &str, event: &str, p: &Value) -> Vec<(&'static str, Value)> {
    let mut st = STATE.lock().unwrap();
    let s = st.entry(pane.to_string()).or_default();
    let session = sid(p);
    let is_child = session
        .as_ref()
        .is_some_and(|x| s.children.contains(x) && s.main.as_ref() != Some(x));
    let mut out = vec![];
    match event {
        "session.created" => {
            let parent = p.pointer("/info/parentID").and_then(Value::as_str);
            match (parent, session) {
                (Some(_), Some(id)) => {
                    s.children.insert(id.clone());
                    s.busy_children.insert(id.clone());
                    out.push(("SubagentStart", json!({"agent_id": id})));
                }
                (None, Some(id)) => {
                    s.main = Some(id.clone());
                    out.push((
                        "SessionStart",
                        json!({"session_id": id, "cwd": p.pointer("/info/directory").or_else(|| p.get("directory")), "source": "startup"}),
                    ));
                }
                _ => {}
            }
        }
        "chat.message" if !is_child => {
            s.idle_pending = false;
            out.push((
                "UserPromptSubmit",
                json!({"prompt": p.get("text"), "session_id": session}),
            ));
        }
        "session.status" => {
            let ty = p
                .pointer("/status/type")
                .and_then(Value::as_str)
                .unwrap_or("");
            match (ty, is_child) {
                ("busy", true) => {
                    if let Some(id) = session {
                        s.busy_children.insert(id);
                    }
                }
                ("busy", false) => out.push(("Working", json!({}))),
                ("retry", _) => {
                    let msg = p
                        .pointer("/status/message")
                        .and_then(Value::as_str)
                        .unwrap_or("retrying");
                    out.push(("Retrying", json!({"message": msg})));
                }
                ("idle", _) => return idle(s, session, is_child),
                _ => {}
            }
        }
        "session.idle" => return idle(s, session, is_child),
        "session.error" if !is_child => {
            let kind = p
                .pointer("/error/name")
                .or_else(|| p.pointer("/error/data/message"))
                .and_then(Value::as_str)
                .unwrap_or("error");
            out.push(("StopFailure", json!({"error_type": kind})));
        }
        "session.compacted" if !is_child => out.push(("PostCompact", json!({}))),
        "tool.execute.before" => {
            let tool = p.get("tool").and_then(Value::as_str).unwrap_or("");
            out.push((
                "PreToolUse",
                json!({"tool_name": tool_name(tool), "tool_input": normalize_args(p.get("args").unwrap_or(&Value::Null)), "tool_use_id": p.get("callID"), "session_id": session}),
            ));
        }
        "tool.execute.after" => {
            let tool = p.get("tool").and_then(Value::as_str).unwrap_or("");
            let exit = p
                .pointer("/metadata/exit")
                .or_else(|| p.pointer("/metadata/exitCode"))
                .cloned();
            let mut input = json!({});
            if let Some(f) = p
                .pointer("/metadata/filepath")
                .or_else(|| p.pointer("/metadata/filePath"))
            {
                input["file_path"] = f.clone();
            }
            out.push((
                "PostToolUse",
                json!({"tool_name": tool_name(tool), "tool_use_id": p.get("callID"), "tool_input": input, "exit_code": exit}),
            ));
        }
        "permission.replied" => {
            if let Some(id) = p
                .get("permissionID")
                .or_else(|| p.get("id"))
                .and_then(Value::as_str)
            {
                out.push(("PermissionResolved", json!({"tool_use_id": id})));
            }
        }
        "file.edited" => {
            if let Some(f) = p.get("file").and_then(Value::as_str) {
                out.push(("FileChanged", json!({"path": f})));
            }
        }
        "message.updated" => {
            let info = p.get("info").cloned().unwrap_or(Value::Null);
            if info.get("role").and_then(Value::as_str) == Some("assistant") {
                out.push(("Usage", info));
            }
        }
        _ => {}
    }
    out
}

fn idle(s: &mut PaneState, session: Option<String>, is_child: bool) -> Vec<(&'static str, Value)> {
    if is_child {
        if let Some(id) = &session {
            s.busy_children.remove(id);
        }
        if s.idle_pending && s.busy_children.is_empty() {
            s.idle_pending = false;
            return vec![(
                "Stop",
                json!({"last_assistant_message": s.last_text.clone(), "session_id": s.main}),
            )];
        }
        return vec![];
    }
    if !s.busy_children.is_empty() {
        // A parent stays working while any child session is busy.
        s.idle_pending = true;
        return vec![];
    }
    s.idle_pending = false;
    vec![(
        "Stop",
        json!({"last_assistant_message": s.last_text.clone(), "session_id": session}),
    )]
}

pub(super) fn on_event(server: &Arc<Server>, pane: &str, h: Harness, event: &str, p: &Value) {
    for (ev, payload) in translate(pane, event, p) {
        match ev {
            "Working" => {
                let run = bound_run(server, pane, h);
                set_execution(
                    server,
                    &run.id,
                    Execution::Working,
                    StateSource::Structured,
                    1.0,
                    None,
                );
            }
            "Retrying" => {
                let run = bound_run(server, pane, h);
                let msg = payload["message"]
                    .as_str()
                    .unwrap_or("retrying")
                    .to_string();
                if super::usage::looks_rate_limited(&msg) {
                    super::usage::rate_limited(server, &run, None, Some(&msg));
                } else {
                    set_execution(
                        server,
                        &run.id,
                        Execution::Working,
                        StateSource::Structured,
                        1.0,
                        Some(format!("retrying: {msg}")),
                    );
                }
            }
            "PermissionResolved" => on_signal(server, pane, h, "PermissionDenied", &payload),
            "FileChanged" => {
                let run = bound_run(server, pane, h);
                if let Some(path) = payload["path"].as_str() {
                    super::route::file_changed(server, &run, path, "modify");
                }
            }
            "Usage" => {
                let run = bound_run(server, pane, h);
                if let Some(t) = text_of(&payload) {
                    STATE
                        .lock()
                        .unwrap()
                        .entry(pane.to_string())
                        .or_default()
                        .last_text = Some(t);
                }
                super::usage::from_opencode(server, &run, &payload);
            }
            "SubagentStart" => {
                let run = bound_run(server, pane, h);
                set_execution(
                    server,
                    &run.id,
                    Execution::Working,
                    StateSource::Structured,
                    1.0,
                    None,
                );
            }
            other => on_signal(server, pane, h, other, &payload),
        }
    }
}

fn text_of(info: &Value) -> Option<String> {
    info.get("summary")
        .and_then(|s| s.get("body"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// `permission.ask` → approval Interaction (native ref = permission id).
pub fn permission_interaction(p: &Value) -> Interaction {
    let kind = p
        .get("type")
        .or_else(|| p.get("permission"))
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let pattern = match p.get("pattern").or_else(|| p.get("patterns")) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(a)) => Some(
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    };
    let command = p
        .pointer("/metadata/command")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or(pattern);
    let mut input = normalize_args(p.get("metadata").unwrap_or(&json!({})));
    if let (Some(c), Some(o)) = (&command, input.as_object_mut()) {
        o.insert("command".into(), json!(c));
    }
    let mut it = harness::interaction_from_hook(
        Harness::Claude,
        "PermissionRequest",
        &json!({"tool_name": tool_name(kind), "tool_input": input, "tool_use_id": p.get("id")}),
    )
    .expect("approval");
    if let Some(t) = p.get("title").and_then(Value::as_str) {
        it.title = t.to_string();
    }
    it
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagents_keep_the_parent_working() {
        let pane = "oc-test-pane";
        let t = |e: &str, p: Value| {
            translate(pane, e, &p)
                .into_iter()
                .map(|(e, _)| e)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            t(
                "session.created",
                json!({"info": {"id": "s1", "directory": "/w"}})
            ),
            vec!["SessionStart"]
        );
        assert_eq!(
            t("chat.message", json!({"sessionID": "s1", "text": "fix it"})),
            vec!["UserPromptSubmit"]
        );
        assert_eq!(
            t(
                "session.created",
                json!({"info": {"id": "c1", "parentID": "s1"}})
            ),
            vec!["SubagentStart"]
        );
        // Child chat is not a user turn; parent idle with a busy child stays working.
        assert!(t("chat.message", json!({"sessionID": "c1", "text": "sub"})).is_empty());
        assert!(t("session.idle", json!({"sessionID": "s1"})).is_empty());
        assert_eq!(t("session.idle", json!({"sessionID": "c1"})), vec!["Stop"]);
        let pre = translate(
            pane,
            "tool.execute.before",
            &json!({"tool": "edit", "callID": "call1", "args": {"filePath": "/w/a.rs", "oldString": "a", "newString": "b"}}),
        );
        assert_eq!(pre[0].0, "PreToolUse");
        assert_eq!(pre[0].1["tool_name"], "Edit");
        assert_eq!(pre[0].1["tool_input"]["file_path"], "/w/a.rs");
        assert_eq!(pre[0].1["tool_use_id"], "call1");
        assert_eq!(
            t(
                "session.status",
                json!({"sessionID": "s1", "status": {"type": "retry", "message": "429 rate limit"}})
            ),
            vec!["Retrying"]
        );
        assert_eq!(
            t(
                "permission.replied",
                json!({"sessionID": "s1", "permissionID": "per_1", "response": "once"})
            ),
            vec!["PermissionResolved"]
        );
    }

    #[test]
    fn permission_ask_maps_to_an_approval() {
        let it = permission_interaction(&json!({
            "id": "per_1", "type": "bash", "pattern": ["rm -rf dist"], "title": "Run rm -rf dist",
            "sessionID": "s1", "callID": "c1", "metadata": {}
        }));
        assert_eq!(it.kind, InteractionKind::Approval);
        assert_eq!(it.native_ref.as_deref(), Some("per_1"));
        let a = it.action.as_ref().unwrap();
        assert_eq!(a.tool, "Bash");
        assert_eq!(a.command.as_deref(), Some("rm -rf dist"));
        assert_eq!(a.risk, Risk::High);
        assert_eq!(it.title, "Run rm -rf dist");
        let d = harness::decision_json(
            Harness::OpenCode,
            &it,
            &Answer {
                decision: Some(Decision::AllowAlways),
                ..Default::default()
            },
        );
        assert_eq!(d, json!({"status": "allow"}));
    }
}
