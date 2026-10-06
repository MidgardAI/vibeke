//! `vibeke hook <harness> <event>` (04 §7.4–7.5): called by harness hook configs. Blocking std
//! I/O, no runtime. Observation fails open; decisions are JSON on stdout only; always exit 0.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const MAX_STDIN: u64 = 4 << 20;

/// Events whose hook may block for a decision (gate-capable, 04 §6.1/§6.2).
fn gate_capable(harness: &str, event: &str, payload: &Value) -> bool {
    match (harness, event) {
        ("claude", "PermissionRequest") | ("codex", "PermissionRequest") => true,
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
    let Ok(mut stream) = UnixStream::connect(&socket) else {
        return 0;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Ok(rd_stream) = stream.try_clone() else {
        return 0;
    };
    let mut rd = BufReader::new(rd_stream);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    if call(&mut stream, &mut rd, 1, "client.hello", json!({"client": "vibeke-hook", "kind": "agent", "token": token, "version": vk_proto::VERSION})).is_none() {
        return 0;
    }
    let params = json!({"harness": harness, "event": event, "payload": payload, "pid": std::os::unix::process::parent_id()});
    if !gate_capable(harness, event, &payload) {
        // The reply may carry a `hook_output` for the harness (collision tracker: queued steering
        // context, an enforced claim's deny). Observation otherwise stays silent on stdout.
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
        return 0;
    }
    // Gate: may wait up to the hook timeout for a decision (or a release on focus).
    let _ = rd
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(1800)));
    let Some(result) = call(&mut stream, &mut rd, 2, "adapter.gate", params) else {
        return 0;
    };
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
    0
}
