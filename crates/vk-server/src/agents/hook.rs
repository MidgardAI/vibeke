//! `vibeke hook <harness> <event>` (04 §7.4–7.5): called by harness hook configs. Blocking std
//! I/O, no runtime. Observation fails open; decisions are JSON on stdout only; always exit 0.
//!
//! Failure policy (04 §2.7):
//! - observation (state signals, `observe`-mode interactions): fail open, print nothing;
//! - enforcement the harness backs (`PermissionRequest` in gate mode): print nothing, so the
//!   harness's own dialog appears;
//! - enforcement only Vibeke provides (policy deny rules on **yolo** runs, via the pre-tool
//!   hook): **fail closed**. With the server unreachable, timed out or answering garbage, the
//!   shim prints `permissionDecision: "ask"` (Claude: its own prompt appears even in bypass
//!   mode) or `"deny"` (Codex, which cannot prompt from a hook) with the reason "Vibeke
//!   unavailable — approve locally?". Never a silent allow. `[agents] fail_closed = false`
//!   (env `VIBEKE_ENFORCE` unset) restores fail-open.
//!
//! A gate request whose connection drops is retried once on a fresh connection (the server
//! re-attaches by `native_ref` and returns an already recorded decision instead of reopening).

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const MAX_STDIN: u64 = 4 << 20;
/// Reason shown when enforcement cannot reach the server.
pub const UNAVAILABLE: &str = "Vibeke unavailable — approve locally?";
/// How long an enforcement decision may take before the shim fails closed.
const ENFORCE_TIMEOUT: Duration = Duration::from_secs(5);

/// Events whose hook may block for a decision (gate-capable, 04 §6.1/§6.2).
fn gate_capable(harness: &str, event: &str, payload: &Value) -> bool {
    match (harness, event) {
        ("claude", "PermissionRequest") | ("codex", "PermissionRequest") => true,
        // Claude MCP elicitation (a server asks the user for input): answered through the hook.
        ("claude", "Elicitation") => true,
        // OpenCode plugin `permission.ask` waits for the decision (04 §6.4).
        ("opencode", "permission.ask") => true,
        ("claude", "PreToolUse") => {
            let tool = payload
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("");
            tool == "AskUserQuestion"
        }
        _ => false,
    }
}

/// Is this a pre-tool hook of a yolo run in a pane that asked for fail-closed enforcement?
/// `Some("ask" | "deny")` is the decision to print when Vibeke cannot be reached.
pub fn enforce_mode(
    harness: &str,
    event: &str,
    payload: &Value,
    env: Option<&str>,
) -> Option<&'static str> {
    if event != "PreToolUse" || !matches!(harness, "claude" | "codex") {
        return None;
    }
    let env = env?;
    if env.is_empty() || env == "0" {
        return None;
    }
    // Only runs without a permission system of their own need Vibeke's boundary.
    let yolo = payload
        .get("permission_mode")
        .and_then(Value::as_str)
        .is_some_and(|m| {
            matches!(
                m,
                "bypassPermissions" | "yolo" | "dangerously-bypass" | "never"
            )
        })
        || payload
            .get("approval_policy")
            .and_then(Value::as_str)
            .is_some_and(|p| p == "never");
    if !yolo {
        return None;
    }
    // A tool the gate path already owns (AskUserQuestion) is not enforcement.
    if gate_capable(harness, event, payload) {
        return None;
    }
    Some(if env == "deny" || harness == "codex" {
        "deny"
    } else {
        "ask"
    })
}

/// The JSON printed when enforcement cannot reach the server.
pub fn fail_closed_json(mode: &str) -> Value {
    json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": if mode == "deny" { "deny" } else { "ask" },
        "permissionDecisionReason": UNAVAILABLE,
    }})
}

fn call(
    stream: &mut UnixStream,
    rd: &mut BufReader<UnixStream>,
    id: u64,
    method: &str,
    params: Value,
) -> Option<Value> {
    let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let mut line = serde_json::to_string(&req).ok()?;
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;
    loop {
        let mut resp = String::new();
        if rd.read_line(&mut resp).ok()? == 0 {
            return None;
        }
        let v: Value = serde_json::from_str(&resp).ok()?;
        if v.get("id").and_then(Value::as_u64) == Some(id) {
            return v.get("result").cloned();
        }
    }
}

/// Why an exchange did not finish.
#[derive(Debug, PartialEq)]
enum Failure {
    /// Nothing reached the server or the reply was lost: try once more.
    Retry,
    /// The server answered something the shim cannot use.
    Fatal,
}

struct Req<'a> {
    socket: &'a str,
    token: &'a str,
    harness: &'a str,
    event: &'a str,
    payload: &'a Value,
    /// A pre-tool enforcement call (short timeout) rather than an interaction gate.
    enforce: bool,
}

/// One connect → hello → signal/gate → (print decision → ack) exchange.
fn exchange(r: &Req) -> Result<(), Failure> {
    let mut stream = UnixStream::connect(r.socket).map_err(|_| Failure::Retry)?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let rd_stream = stream.try_clone().map_err(|_| Failure::Retry)?;
    let mut rd = BufReader::new(rd_stream);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    if call(&mut stream, &mut rd, 1, "client.hello", json!({"client": "vibeke-hook", "kind": "agent", "token": r.token, "version": vk_proto::VERSION})).is_none() {
        return Err(Failure::Retry);
    }
    let params = json!({"harness": r.harness, "event": r.event, "payload": r.payload, "pid": std::os::unix::process::parent_id()});
    if !r.enforce && !gate_capable(r.harness, r.event, r.payload) {
        // The reply may carry a `hook_output` for the harness (collision tracker: queued
        // steering context, an enforced claim's deny). Observation otherwise stays silent.
        if let Some(out) = call(&mut stream, &mut rd, 2, "adapter.signal", params)
            .as_ref()
            .and_then(|r| r.get("hook_output"))
            .filter(|o| o.is_object())
        {
            let mut stdout = std::io::stdout();
            let _ = stdout
                .write_all(serde_json::to_string(out).unwrap_or_default().as_bytes())
                .and_then(|_| stdout.write_all(b"\n"))
                .and_then(|_| stdout.flush());
        }
        return Ok(());
    }
    // Gate: may wait up to the hook timeout for a decision (or a release on focus); an
    // enforcement decision is immediate or fails closed.
    let wait = if r.enforce {
        ENFORCE_TIMEOUT
    } else {
        Duration::from_secs(1800)
    };
    let _ = rd.get_ref().set_read_timeout(Some(wait));
    let Some(result) = call(&mut stream, &mut rd, 2, "adapter.gate", params) else {
        return Err(Failure::Retry);
    };
    if !result.is_object() {
        return Err(Failure::Fatal);
    }
    if let Some(decision) = result.get("decision").filter(|d| d.is_object()) {
        // Hand the decision to the harness first; ack only once stdout took it. A failed write
        // is not acked, so the server reconciles instead of recording a false delivery.
        let out = serde_json::to_string(decision).unwrap_or_default();
        let mut stdout = std::io::stdout();
        let written = stdout
            .write_all(out.as_bytes())
            .and_then(|_| stdout.write_all(b"\n"))
            .and_then(|_| stdout.flush())
            .is_ok();
        if written
            && let (Some(i), Some(k)) = (result.get("interaction"), result.get("idempotency_key"))
        {
            let _ = call(
                &mut stream,
                &mut rd,
                3,
                "adapter.delivery_ack",
                json!({"interaction": i, "idempotency_key": k, "applied": true}),
            );
        }
    }
    Ok(())
}

pub fn main(args: &[String]) -> i32 {
    let (Some(harness), Some(event)) = (args.first(), args.get(1)) else {
        return 0;
    };
    if std::env::var("VIBEKE").as_deref() != Ok("1") {
        return 0;
    }
    // A headless run (01 §3.3): the server's adapter owns the harness's stdio stream and
    // already sees everything the hooks would report; reporting twice would double events.
    if std::env::var("VIBEKE_HEADLESS_OWNER").as_deref() == Ok("1") {
        return 0;
    }
    let (Ok(socket), Ok(token)) = (
        std::env::var("VIBEKE_SOCKET"),
        std::env::var("VIBEKE_PANE_TOKEN"),
    ) else {
        return 0;
    };
    let mut input = String::new();
    let _ = std::io::stdin().take(MAX_STDIN).read_to_string(&mut input);
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    if let Some(dir) = std::env::var_os("VIBEKE_HOOK_TEE") {
        // Golden-corpus recording mode (04 §12.1).
        let path = std::path::Path::new(&dir).join(format!("{harness}.jsonl"));
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{}", json!({"event": event, "payload": payload}));
        }
    }
    let enforce = enforce_mode(
        harness,
        event,
        &payload,
        std::env::var("VIBEKE_ENFORCE").ok().as_deref(),
    );
    let req = Req {
        socket: &socket,
        token: &token,
        harness,
        event,
        payload: &payload,
        enforce: enforce.is_some(),
    };
    let gate = enforce.is_some() || gate_capable(harness, event, &payload);
    // Gate requests are retried once on a fresh connection; signals are not worth it.
    let attempts = if gate { 2 } else { 1 };
    let mut outcome = Err(Failure::Retry);
    for n in 0..attempts {
        outcome = exchange(&req);
        match outcome {
            Err(Failure::Retry) if n + 1 < attempts => {
                std::thread::sleep(Duration::from_millis(150));
            }
            _ => break,
        }
    }
    if outcome.is_err()
        && let Some(mode) = enforce
    {
        // Enforcement only Vibeke provides: never a silent allow (04 §2.7).
        let mut stdout = std::io::stdout();
        let _ = writeln!(stdout, "{}", fail_closed_json(mode));
        let _ = stdout.flush();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pre(mode: &str) -> Value {
        json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf /"}, "permission_mode": mode})
    }

    #[test]
    fn enforcement_applies_to_yolo_pre_tool_hooks_only() {
        assert_eq!(
            enforce_mode("claude", "PreToolUse", &pre("bypassPermissions"), Some("1")),
            Some("ask")
        );
        assert_eq!(
            enforce_mode(
                "claude",
                "PreToolUse",
                &pre("bypassPermissions"),
                Some("deny")
            ),
            Some("deny")
        );
        // Codex cannot prompt from a hook: deny.
        assert_eq!(
            enforce_mode(
                "codex",
                "PreToolUse",
                &json!({"approval_policy": "never"}),
                Some("1")
            ),
            Some("deny")
        );
        // A run with its own permission system keeps failing open to the harness's dialog.
        assert_eq!(
            enforce_mode("claude", "PreToolUse", &pre("default"), Some("1")),
            None
        );
        // Not enforcement: other events, other harnesses, no opt-in, the question gate.
        assert_eq!(
            enforce_mode(
                "claude",
                "PostToolUse",
                &pre("bypassPermissions"),
                Some("1")
            ),
            None
        );
        assert_eq!(
            enforce_mode("gemini", "PreToolUse", &pre("bypassPermissions"), Some("1")),
            None
        );
        assert_eq!(
            enforce_mode("claude", "PreToolUse", &pre("bypassPermissions"), None),
            None
        );
        assert_eq!(
            enforce_mode("claude", "PreToolUse", &pre("bypassPermissions"), Some("0")),
            None
        );
        let q = json!({"tool_name": "AskUserQuestion", "permission_mode": "bypassPermissions"});
        assert_eq!(enforce_mode("claude", "PreToolUse", &q, Some("1")), None);
    }

    #[test]
    fn fail_closed_json_is_a_pretooluse_ask_or_deny_with_the_reason() {
        let v = fail_closed_json("ask");
        let o = &v["hookSpecificOutput"];
        assert_eq!(o["hookEventName"], "PreToolUse");
        assert_eq!(o["permissionDecision"], "ask");
        assert_eq!(o["permissionDecisionReason"], UNAVAILABLE);
        assert_eq!(
            fail_closed_json("deny")["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
        assert!(UNAVAILABLE.contains("approve locally"));
    }

    #[test]
    fn elicitation_is_gate_capable() {
        assert!(gate_capable("claude", "Elicitation", &Value::Null));
        assert!(!gate_capable("claude", "CwdChanged", &Value::Null));
        assert!(!gate_capable("codex", "Elicitation", &Value::Null));
    }

    #[test]
    fn an_unreachable_server_is_a_retryable_failure() {
        let payload = json!({});
        let r = Req {
            socket: "/nonexistent/vibeke.sock",
            token: "t",
            harness: "claude",
            event: "PreToolUse",
            payload: &payload,
            enforce: true,
        };
        assert_eq!(exchange(&r), Err(Failure::Retry));
    }
}
