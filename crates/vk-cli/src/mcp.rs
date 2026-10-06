//! `vibeke mcp`: a stdio MCP server (spec 06 B7) exposing previews and the agents' headless
//! browser as tools. JSON-RPC 2.0, one message per line (MCP stdio transport); protocol
//! versions 2025-11-25 (preferred), 2025-06-18, 2025-03-26 and 2024-11-05.
//!
//! Every tool call is forwarded to the Vibeke server as the matching API method. The process
//! runs inside the agent's pane, so its connection is pane-scoped (by `VIBEKE_PANE_TOKEN`, or by
//! process ancestry): the agent can only use this machine's previews and its own browser
//! sessions. Tool failures come back as tool results with `isError: true` (with the Vibeke error
//! kind, e.g. `destination_denied` or `human_control`), never as protocol errors.

use crate::client::{CallError, Client};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where tool calls go: the Vibeke server (or a fake in tests).
pub trait Backend: Send {
    fn call<'a>(
        &'a mut self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, CallError>>;
}

impl<S> Backend for Client<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    fn call<'a>(
        &'a mut self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, CallError>> {
        Box::pin(Client::call(self, method, params))
    }
}

/// A backend that (re)connects on demand: `connect` yields a fresh, `client.hello`'d client.
pub struct Reconnecting<S, F> {
    connect: F,
    client: Option<Client<S>>,
}

impl<S, F> Reconnecting<S, F> {
    pub fn new(connect: F) -> Self {
        Reconnecting {
            connect,
            client: None,
        }
    }
}

impl<S, F> Backend for Reconnecting<S, F>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
    F: FnMut() -> BoxFuture<'static, anyhow::Result<Client<S>>> + Send,
{
    fn call<'a>(
        &'a mut self,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, CallError>> {
        Box::pin(async move {
            for attempt in 0..2 {
                if self.client.is_none() {
                    let mut c = (self.connect)().await.map_err(CallError::Io)?;
                    c.hello("mcp").await?;
                    self.client = Some(c);
                }
                let c = self.client.as_mut().expect("connected");
                match Client::call(c, method, params.clone()).await {
                    Err(CallError::Io(e)) if attempt == 0 => {
                        // Server restarted (e.g. `vibeke update`): reconnect once.
                        let _ = e;
                        self.client = None;
                    }
                    r => return r,
                }
            }
            unreachable!("returns within two attempts")
        })
    }
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

fn session_prop() -> Value {
    json!({"type": "string", "description": "Browser session id from browser_open (e.g. \"b3\")."})
}

/// The tool list (`tools/list`).
pub fn tools() -> Vec<Value> {
    let s = session_prop;
    vec![
        json!({"name": "preview_declare", "title": "Declare a preview",
               "description": "Declare a dev server running on this machine (e.g. after starting `pnpm dev`) so the user can open it and browser sessions may load it. Only declared previews are reachable from the headless browser.",
               "inputSchema": obj(json!({"port": {"type": "integer", "minimum": 1, "maximum": 65535}, "path": {"type": "string"}, "label": {"type": "string"}}), &["port"])}),
        json!({"name": "preview_list", "title": "List previews",
               "description": "Previews (dev servers) on this machine, with their handles (v1, v2, …) and status.",
               "inputSchema": obj(json!({"all": {"type": "boolean", "description": "Include undeclared suggestions."}}), &[]),
               "annotations": {"readOnlyHint": true}}),
        json!({"name": "browser_open", "title": "Open a browser session",
               "description": "Open an isolated headless browser session (fresh cookies) on this machine, optionally loading a preview (handle like \"v1\") or a URL. Only this machine's declared previews (and configured allowed hosts) are reachable; other loopback ports, private and metadata addresses are refused. Returns the session id.",
               "inputSchema": obj(json!({"preview": {"type": "string"}, "url": {"type": "string"}, "viewport": {"type": "string", "description": "WxH in CSS px, e.g. 390x844"}, "color_scheme": {"type": "string", "enum": ["light", "dark"]}}), &[])}),
        json!({"name": "browser_navigate", "title": "Navigate",
               "description": "Load a URL or a path (\"/settings\", relative to the current origin) and wait for the load event.",
               "inputSchema": obj(json!({"session": s(), "url": {"type": "string"}}), &["session", "url"])}),
        json!({"name": "browser_click", "title": "Click",
               "description": "Click an element by CSS selector or `text=Label`, or at viewport coordinates x,y (CSS px).",
               "inputSchema": obj(json!({"session": s(), "selector": {"type": "string"}, "x": {"type": "number"}, "y": {"type": "number"}}), &["session"])}),
        json!({"name": "browser_type", "title": "Type text",
               "description": "Type text into the element matching `selector` (or the focused element). `submit` presses Enter afterwards; `clear` replaces the current value.",
               "inputSchema": obj(json!({"session": s(), "selector": {"type": "string"}, "text": {"type": "string"}, "submit": {"type": "boolean"}, "clear": {"type": "boolean"}}), &["session", "text"])}),
        json!({"name": "browser_press", "title": "Press a key",
               "description": "Press a key or chord: enter, tab, escape, ArrowDown, ctrl+a, shift+tab.",
               "inputSchema": obj(json!({"session": s(), "key": {"type": "string"}}), &["session", "key"])}),
        json!({"name": "browser_wait", "title": "Wait",
               "description": "Wait for `load`, `networkidle`, `selector:<css>` or `ms:<n>`.",
               "inputSchema": obj(json!({"session": s(), "for": {"type": "string"}, "timeout_ms": {"type": "integer"}}), &["session", "for"])}),
        json!({"name": "browser_eval", "title": "Evaluate JavaScript",
               "description": "Evaluate a JavaScript expression in the page and return its JSON value (size-limited). Requires the browser.script capability (preview.browser_script = true).",
               "inputSchema": obj(json!({"session": s(), "expression": {"type": "string"}}), &["session", "expression"])}),
        json!({"name": "browser_screenshot", "title": "Screenshot",
               "description": "Take a PNG screenshot (viewport, full page, or one element). Returns the image plus metadata: blob id, path on this machine, environment (headless, machine).",
               "inputSchema": obj(json!({"session": s(), "full_page": {"type": "boolean"}, "selector": {"type": "string"}}), &["session"])}),
        json!({"name": "browser_snapshot", "title": "Page snapshot",
               "description": "A text snapshot of the page: the accessibility tree (default, best for finding things to click), visible text, or HTML.",
               "inputSchema": obj(json!({"session": s(), "format": {"type": "string", "enum": ["a11y", "text", "html"]}, "selector": {"type": "string"}}), &["session"]),
               "annotations": {"readOnlyHint": true}}),
        json!({"name": "browser_console", "title": "Console log",
               "description": "Console messages and uncaught exceptions captured in the session.",
               "inputSchema": obj(json!({"session": s(), "level": {"type": "string", "enum": ["error", "warn", "all"]}, "since_ms": {"type": "integer"}}), &["session"]),
               "annotations": {"readOnlyHint": true}}),
        json!({"name": "browser_network", "title": "Network log",
               "description": "Requests made by the session, with status, errors and requests blocked by Vibeke's destination policy (with the reason).",
               "inputSchema": obj(json!({"session": s(), "failed_only": {"type": "boolean"}, "since_ms": {"type": "integer"}}), &["session"]),
               "annotations": {"readOnlyHint": true}}),
        json!({"name": "browser_diff", "title": "Compare two screenshots",
               "description": "Visual diff of two screenshots (ids or handles like \"s3\" from browser_screenshot): changed pixel ratio, changed regions and a diff image (changes in red). Screenshots from different environments (headless vs a browser pane) are refused unless force is set.",
               "inputSchema": obj(json!({"a": {"type": "string"}, "b": {"type": "string"}, "threshold": {"type": "number", "minimum": 0, "maximum": 1, "description": "Per-channel tolerance (fraction), default 0.1."}, "force": {"type": "boolean"}}), &["a", "b"])}),
        json!({"name": "browser_close", "title": "Close the session",
               "description": "Close a browser session (its cookies and storage are discarded).",
               "inputSchema": obj(json!({"session": s()}), &["session"])}),
    ]
}

/// Tool name → (API method, params).
pub fn map_tool(name: &str, args: &Value) -> Option<(&'static str, Value)> {
    let mut a = args.clone();
    if !a.is_object() {
        a = json!({});
    }
    let method = match name {
        "preview_declare" => "preview.declare",
        "preview_list" => "preview.list",
        "browser_open" => "browser.open",
        "browser_navigate" => "browser.navigate",
        "browser_click" => "browser.click",
        "browser_type" => "browser.type",
        "browser_press" => "browser.press",
        "browser_wait" => "browser.wait",
        "browser_eval" => "browser.eval",
        "browser_screenshot" => {
            a["inline"] = json!(true);
            "browser.screenshot"
        }
        "browser_snapshot" => "browser.snapshot",
        "browser_console" => "browser.console",
        "browser_network" => "browser.network",
        "browser_close" => "browser.close",
        "browser_diff" => {
            a["inline"] = json!(true);
            "browser.diff"
        }
        _ => return None,
    };
    Some((method, a))
}

fn text(t: impl Into<String>) -> Value {
    json!({"type": "text", "text": t.into()})
}

/// Turn an API result into MCP tool content.
pub fn tool_result(name: &str, r: Result<Value, CallError>) -> Value {
    match r {
        Ok(mut v) => {
            let mut content = Vec::new();
            if (name == "browser_screenshot" || name == "browser_diff")
                && let Some(data) = v.as_object_mut().and_then(|o| o.remove("data_b64"))
            {
                content.push(json!({"type": "image", "data": data, "mimeType": v["mime"].as_str().unwrap_or("image/png")}));
            }
            if name == "browser_snapshot" {
                let body = v["content"].as_str().unwrap_or("").to_string();
                let head = format!(
                    "{} snapshot of {}{}",
                    v["format"].as_str().unwrap_or("a11y"),
                    v["url"].as_str().unwrap_or(""),
                    if v["truncated"] == true {
                        " (truncated)"
                    } else {
                        ""
                    }
                );
                content.push(text(format!("{head}\n\n{body}")));
            } else {
                content.push(text(serde_json::to_string_pretty(&v).unwrap_or_default()));
            }
            json!({"content": content, "isError": false})
        }
        Err(CallError::Rpc(e)) => {
            let mut msg = format!("{}: {}", e.data.kind, e.message);
            if !e.data.details.is_null() {
                msg.push_str(&format!("\ndetails: {}", e.data.details));
            }
            json!({"content": [text(msg)], "isError": true})
        }
        Err(CallError::Io(e)) => {
            json!({"content": [text(format!("vibeke server unavailable: {e:#}"))], "isError": true})
        }
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Handle one JSON-RPC message; `None` for notifications.
pub async fn handle(msg: &Value, backend: &mut dyn Backend) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str);
    let Some(method) = method else {
        // A response to something we never send, or garbage.
        return id.map(|id| rpc_error(id, -32600, "invalid request"));
    };
    let id = id?; // notifications (`notifications/initialized`, `notifications/cancelled`, …)
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result = match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = PROTOCOL_VERSIONS
                .iter()
                .find(|v| **v == asked)
                .copied()
                .unwrap_or(PROTOCOL_VERSIONS[0]);
            json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "vibeke", "title": "Vibeke", "version": vk_proto::VERSION},
                "instructions": "Vibeke previews and a headless browser on this machine. After starting a dev server, call preview_declare (or preview_list). Then browser_open {preview} → browser_snapshot to find elements → browser_click/browser_type → browser_screenshot to verify. Check browser_console/browser_network for errors. Requests outside this machine's declared previews are blocked by policy (see the error reason). If a call fails with human_control, the user has taken over the session: wait and retry later.",
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools": tools()}),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let Some((m, p)) = map_tool(name, &args) else {
                return Some(rpc_error(id, -32602, &format!("unknown tool: {name}")));
            };
            tool_result(name, backend.call(m, p).await)
        }
        "resources/list" => json!({"resources": []}),
        "prompts/list" => json!({"prompts": []}),
        other => return Some(rpc_error(id, -32601, &format!("method not found: {other}"))),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// Serve MCP over a line-delimited stream until EOF.
pub async fn serve<R, W>(reader: R, mut writer: W, backend: &mut dyn Backend) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Err(e) => Some(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))),
            Ok(Value::Array(batch)) => {
                let mut out = Vec::new();
                for m in &batch {
                    if let Some(r) = handle(m, backend).await {
                        out.push(r);
                    }
                }
                (!out.is_empty()).then_some(Value::Array(out))
            }
            Ok(m) => handle(&m, backend).await,
        };
        if let Some(r) = reply {
            let mut s = serde_json::to_string(&r)?;
            s.push('\n');
            writer.write_all(s.as_bytes()).await?;
            writer.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, BufReader};

    /// A fake Vibeke server on the far end of a duplex pipe: records calls, answers a few.
    fn fake_server(
        stream: tokio::io::DuplexStream,
        seen: Arc<Mutex<Vec<(String, Value)>>>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let (rd, mut wr) = tokio::io::split(stream);
            let mut lines = BufReader::new(rd).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let req: Value = serde_json::from_str(&l).unwrap();
                let method = req["method"].as_str().unwrap().to_string();
                let params = req["params"].clone();
                seen.lock().unwrap().push((method.clone(), params.clone()));
                let resp = match method.as_str() {
                    "client.hello" => json!({"result": {"capabilities": ["pane"]}}),
                    "browser.open" => {
                        json!({"result": {"session": "b1", "environment": {"kind": "remote_headless"}}})
                    }
                    "browser.screenshot" => {
                        json!({"result": {"session": "b1", "blob": "abc", "mime": "image/png", "data_b64": "iVBORw0KGgo=", "meta": {"environment": {"kind": "remote_headless"}}}})
                    }
                    "browser.snapshot" => {
                        json!({"result": {"format": "a11y", "url": "http://localhost:5173/", "content": "button \"Save\"\n", "truncated": false}})
                    }
                    "browser.click" => {
                        json!({"error": {"code": -32004, "message": "human_control: a human has taken over browser session b1", "data": {"kind": "human_control", "details": {"session": "b1"}, "retryable": true}}})
                    }
                    "browser.navigate" => {
                        json!({"error": {"code": -32003, "message": "destination_denied: http://127.0.0.1:5432/", "data": {"kind": "destination_denied", "details": {"reason": "loopback_port_not_a_preview"}, "retryable": false}}})
                    }
                    _ => json!({"result": {}}),
                };
                let mut out = resp;
                out["jsonrpc"] = json!("2.0");
                out["id"] = req["id"].clone();
                let mut s = serde_json::to_string(&out).unwrap();
                s.push('\n');
                wr.write_all(s.as_bytes()).await.unwrap();
            }
        })
    }

    async fn session(input: &str) -> (Vec<Value>, Vec<(String, Value)>) {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let seen = Arc::new(Mutex::new(Vec::new()));
        fake_server(theirs, seen.clone());
        let mut client = Client::new(ours);
        client.hello("mcp").await.unwrap();
        // MCP stdio over another pipe.
        let (mut harness, mcp_side) = tokio::io::duplex(1 << 20);
        let (mrd, mwr) = tokio::io::split(mcp_side);
        let task = tokio::spawn(async move {
            serve(BufReader::new(mrd), mwr, &mut client).await.unwrap();
        });
        harness.write_all(input.as_bytes()).await.unwrap();
        harness.shutdown().await.unwrap();
        let mut out = String::new();
        harness.read_to_string(&mut out).await.unwrap();
        task.await.unwrap();
        let replies = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let seen = seen.lock().unwrap().clone();
        (replies, seen)
    }

    fn line(v: Value) -> String {
        format!("{}\n", serde_json::to_string(&v).unwrap())
    }

    #[tokio::test]
    async fn initialize_list_and_call() {
        let input = [
            line(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "2"}}})),
            line(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
            line(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})),
            line(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "browser_open", "arguments": {"preview": "v1"}}})),
            line(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "browser_screenshot", "arguments": {"session": "b1"}}})),
            line(json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {"name": "browser_click", "arguments": {"session": "b1", "selector": "text=Save"}}})),
            line(json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "browser_navigate", "arguments": {"session": "b1", "url": "http://127.0.0.1:5432/"}}})),
            line(json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "browser_snapshot", "arguments": {"session": "b1"}}})),
            line(json!({"jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {"name": "nope", "arguments": {}}})),
            line(json!({"jsonrpc": "2.0", "id": 9, "method": "ping"})),
            line(json!({"jsonrpc": "2.0", "id": 10, "method": "bogus/method"})),
            "this is not json\n".to_string(),
        ]
        .concat();
        let (r, seen) = session(&input).await;
        // No reply to the notification.
        assert_eq!(r.len(), 11, "{r:#?}");
        assert_eq!(r[0]["id"], 1);
        assert_eq!(r[0]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(r[0]["result"]["serverInfo"]["name"], "vibeke");
        assert!(r[0]["result"]["capabilities"]["tools"].is_object());
        let names: Vec<&str> = r[1]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        for want in [
            "preview_declare",
            "preview_list",
            "browser_open",
            "browser_navigate",
            "browser_click",
            "browser_type",
            "browser_press",
            "browser_eval",
            "browser_screenshot",
            "browser_snapshot",
            "browser_console",
            "browser_network",
            "browser_close",
            "browser_diff",
        ] {
            assert!(names.contains(&want), "{want}");
        }
        for t in r[1]["result"]["tools"].as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        }
        assert_eq!(r[2]["result"]["isError"], false);
        assert!(
            r[2]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("\"b1\"")
        );
        // Screenshot → image content first, metadata text without the base64.
        let c = &r[3]["result"]["content"];
        assert_eq!(c[0]["type"], "image");
        assert_eq!(c[0]["mimeType"], "image/png");
        assert_eq!(c[0]["data"], "iVBORw0KGgo=");
        assert!(!c[1]["text"].as_str().unwrap().contains("iVBORw0KGgo="));
        assert!(c[1]["text"].as_str().unwrap().contains("remote_headless"));
        // API errors are tool errors with the Vibeke kind.
        assert_eq!(r[4]["result"]["isError"], true);
        assert!(
            r[4]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("human_control:")
        );
        assert!(
            r[5]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("loopback_port_not_a_preview")
        );
        assert!(
            r[6]["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("button \"Save\"")
        );
        assert_eq!(r[7]["error"]["code"], -32602);
        assert_eq!(r[8]["result"], json!({}));
        assert_eq!(r[9]["error"]["code"], -32601);
        assert_eq!(r[10]["error"]["code"], -32700);
        // What reached the server.
        let methods: Vec<&str> = seen.iter().map(|(m, _)| m.as_str()).collect();
        assert_eq!(
            methods,
            [
                "client.hello",
                "browser.open",
                "browser.screenshot",
                "browser.click",
                "browser.navigate",
                "browser.snapshot"
            ]
        );
        assert_eq!(seen[1].1, json!({"preview": "v1"}));
        assert_eq!(seen[2].1, json!({"session": "b1", "inline": true}));
    }

    #[tokio::test]
    async fn unknown_protocol_version_gets_ours_and_batches_work() {
        let input = line(json!([
            {"jsonrpc": "2.0", "id": "a", "method": "initialize", "params": {"protocolVersion": "1999-01-01"}},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": "b", "method": "tools/call", "params": {"name": "preview_declare", "arguments": {"port": 5173, "label": "web"}}}
        ]));
        let (r, seen) = session(&input).await;
        assert_eq!(r.len(), 1);
        let batch = r[0].as_array().unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0]["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert_eq!(batch[1]["id"], "b");
        assert_eq!(
            seen[1],
            (
                "preview.declare".to_string(),
                json!({"port": 5173, "label": "web"})
            )
        );
    }

    #[tokio::test]
    async fn reconnecting_backend_retries_once_after_disconnect() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let connects = Arc::new(Mutex::new(0));
        let (s2, c2) = (seen.clone(), connects.clone());
        let mut b = Reconnecting::new(move || {
            let seen = s2.clone();
            let connects = c2.clone();
            Box::pin(async move {
                *connects.lock().unwrap() += 1;
                let (ours, theirs) = tokio::io::duplex(1 << 16);
                if *connects.lock().unwrap() == 1 {
                    // First server dies right after hello.
                    tokio::spawn(async move {
                        let (rd, mut wr) = tokio::io::split(theirs);
                        let mut lines = BufReader::new(rd).lines();
                        if let Ok(Some(l)) = lines.next_line().await {
                            let req: Value = serde_json::from_str(&l).unwrap();
                            let s = format!(
                                "{}\n",
                                json!({"jsonrpc": "2.0", "id": req["id"], "result": {}})
                            );
                            wr.write_all(s.as_bytes()).await.unwrap();
                        }
                    });
                } else {
                    fake_server(theirs, seen);
                }
                Ok(Client::new(ours))
            }) as BoxFuture<'static, anyhow::Result<Client<tokio::io::DuplexStream>>>
        });
        let v = Backend::call(&mut b, "browser.open", json!({}))
            .await
            .unwrap();
        assert_eq!(v["session"], "b1");
        assert_eq!(*connects.lock().unwrap(), 2);
    }
}
