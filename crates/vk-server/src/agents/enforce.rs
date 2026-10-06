//! Vibeke-only enforcement at the pre-tool hook (04 §2.7, §7.4): policy `deny` (and `ask`) rules
//! on **yolo** runs, where the harness has no permission system of its own to fall back on.
//!
//! The hook shim sends `PreToolUse` of a yolo run through `adapter.gate`
//! ([`super::hook::enforce_mode`]); [`pre_tool`] records the signal (state tracking is unchanged),
//! evaluates the policy ([`crate::policy_api::evaluate`]) and answers with Claude's
//! `permissionDecision` JSON: `deny` for a matching deny rule, `ask` for a matching ask rule
//! (the harness's own prompt appears even in bypass mode), nothing otherwise. A yolo run with
//! no matching rule is never slowed or prompted. When the server cannot be reached the shim fails
//! closed on its own ([`super::hook::fail_closed_json`]).

use super::*;
use crate::policy_api::{self, Action};
use std::path::Path;

/// `permissionDecision` JSON for a PreToolUse hook (`deny` | `ask`).
pub fn decision_json(effect: &str, reason: &str) -> Value {
    json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": if effect == "deny" { "deny" } else { "ask" },
        "permissionDecisionReason": reason,
    }})
}

/// The policy action a pre-tool payload describes.
pub fn action_of(p: &Value) -> Action {
    let input = p.get("tool_input").cloned().unwrap_or(Value::Null);
    let command = input.get("command").and_then(|c| {
        c.as_str().map(str::to_string).or_else(|| {
            c.as_array().map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        })
    });
    let paths: Vec<String> = ["file_path", "path", "notebook_path"]
        .iter()
        .filter_map(|k| input.get(*k).and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    Action {
        tool: p
            .get("tool_name")
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string(),
        command,
        paths,
        url: input.get("url").and_then(Value::as_str).map(str::to_string),
    }
}

/// Pure verdict: the hook JSON for a policy decision, or `None` to let the tool run. Only an
/// explicit matching rule decides; "no rule matched" (`ask` without a rule) is not enforcement.
pub fn verdict(effect: &str, rule: Option<&str>) -> Option<Value> {
    let rule = rule?;
    match effect {
        "deny" => Some(decision_json(
            "deny",
            &format!("Denied by Vibeke policy rule {rule}"),
        )),
        "ask" => Some(decision_json(
            "ask",
            &format!("Vibeke policy rule {rule} asks for confirmation"),
        )),
        _ => None,
    }
}

/// `adapter.gate` for a `PreToolUse` hook.
pub async fn pre_tool(server: &Arc<Server>, pane: &str, h: Harness, p: &Value) -> R {
    // State tracking as for any other signal.
    on_signal(server, pane, h, "PreToolUse", p);
    let run = server.with_core(|c| c.run_for_pane(pane).cloned());
    let yolo = run.as_ref().is_some_and(|r| r.yolo)
        || p.get("permission_mode")
            .and_then(Value::as_str)
            .is_some_and(|m| m == "bypassPermissions");
    if !yolo {
        return Ok(json!({"decision": null}));
    }
    let action = action_of(p);
    let (cwd, repo) = server.with_core(|c| {
        let r = run.as_ref();
        let cwd = r
            .and_then(|r| r.cwd.clone())
            .or_else(|| c.pane(pane).and_then(|x| x.cwd.clone()));
        let repo = r
            .and_then(|r| r.task.clone())
            .and_then(|t| c.task(&t).map(|t| t.repo_root.clone()));
        (cwd, repo)
    });
    let d = policy_api::evaluate(
        server,
        &action,
        cwd.as_deref().map(Path::new),
        repo.as_deref().map(Path::new),
    );
    let rule = d.rule.as_ref().map(|r| r.id.clone());
    let Some(json) = verdict(&d.effect, rule.as_deref()) else {
        return Ok(json!({"decision": null}));
    };
    if let Some(r) = &run {
        update_run(server, &r.id, |r, tx| {
            tx.event(
                "agent.tool_blocked",
                json!({"run": r.id, "pane": r.pane}),
                json!({"tool": action.tool, "effect": d.effect, "rule": rule, "command": action.command.as_ref().map(|c| c.chars().take(200).collect::<String>())}),
            );
        });
    }
    Ok(json!({"decision": json}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_and_ask_rules_decide_and_everything_else_runs() {
        let v = verdict("deny", Some("r-1")).unwrap();
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            v["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("r-1")
        );
        assert_eq!(
            verdict("ask", Some("r-2")).unwrap()["hookSpecificOutput"]["permissionDecision"],
            "ask"
        );
        // An allow rule, or "no rule matched", never prompts a yolo run.
        assert!(verdict("allow", Some("r-3")).is_none());
        assert!(verdict("ask", None).is_none());
        assert!(verdict("deny", None).is_none());
    }

    #[test]
    fn actions_come_from_the_tool_input() {
        let a =
            action_of(&json!({"tool_name": "Bash", "tool_input": {"command": "git push --force"}}));
        assert_eq!(
            (a.tool.as_str(), a.command.as_deref()),
            ("Bash", Some("git push --force"))
        );
        let a = action_of(&json!({"tool_name": "Edit", "tool_input": {"file_path": "/r/.env"}}));
        assert_eq!(a.paths, vec!["/r/.env".to_string()]);
        let a = action_of(
            &json!({"tool_name": "shell", "tool_input": {"command": ["bash", "-lc", "ls"]}}),
        );
        assert_eq!(a.command.as_deref(), Some("bash -lc ls"));
        let a =
            action_of(&json!({"tool_name": "WebFetch", "tool_input": {"url": "https://x.dev"}}));
        assert_eq!(a.url.as_deref(), Some("https://x.dev"));
    }
}
