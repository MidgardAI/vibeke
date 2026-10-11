//! Herdr's socket wire format (07 §8.3): one newline-terminated JSON request per connection,
//! `{"id": "<string>", "method": "...", "params": {...}}`; success
//! `{"id", "result": {"type": "<result_type>", …}}`; error
//! `{"id", "error": {"code": "<snake_case>", "message"}}`; streamed events
//! `{"event": "<snake_case>", "data": {...}}`. No JSON-RPC envelope and no handshake.
//!
//! Malformed input follows Herdr 0.9.3 (`src/api/server.rs`): a line that is not a valid
//! request (bad JSON, a non-string id, no method, params of the wrong type, an unknown method)
//! answers `invalid_request` with the message `invalid request: <reason>`, echoing the id only
//! when the line is an object with a string `id`. A line over [`MAX_LINE`] bytes, a line that is
//! not UTF-8 and an empty line get no answer: Herdr closes the connection.

use serde_json::{Map, Value, json};

/// Longest accepted request line (bytes), Herdr's `MAX_INITIAL_REQUEST_BYTES`.
pub const MAX_LINE: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: String,
    pub method: String,
    pub params: Value,
    /// Vibeke extension: the invocation's broker token (`VIBEKE_HERDR_TOKEN`), sent by the
    /// private `herdr` launcher as a top-level `vibeke_token` so a process the server cannot
    /// place in the invocation's process tree (a daemon that left its session) still proves it
    /// belongs to the invocation. Herdr ignores unknown top-level keys.
    pub token: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WireError {
    pub code: String,
    pub message: String,
}

impl WireError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        WireError {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// Herdr's `invalid_request` error: `invalid request: <reason>`.
pub fn invalid_request(reason: impl std::fmt::Display) -> WireError {
    WireError::new("invalid_request", format!("invalid request: {reason}"))
}

/// The error for a method Herdr 0.9.3 does not have: its typed request parse fails on the
/// method tag, so it answers `invalid_request` (Herdr's message also lists every method).
pub fn unknown_method(method: &str) -> WireError {
    invalid_request(format!("unknown variant `{method}`"))
}

/// Herdr's answer to the retired `pane.graphics.*` methods (`retired_pane_graphics_method_error`).
pub fn retired_method(method: &str) -> Option<WireError> {
    matches!(
        method,
        "pane.graphics.info" | "pane.graphics.set" | "pane.graphics.clear" | "pane.graphics.stream"
    )
    .then(|| WireError::new("unknown_method", format!("unknown method: {method}")))
}

/// Whether Herdr answers a request line at all: it closes the connection without a response
/// for an over-long line, a line that is not UTF-8 and a blank line.
pub fn answers(line: &[u8]) -> bool {
    line.len() <= MAX_LINE && std::str::from_utf8(line).is_ok_and(|t| !t.trim().is_empty())
}

/// Parse one request line. On error, returns the id to echo (the request's string id when it
/// can be recovered, `""` otherwise) with the error.
pub fn parse_request(line: &[u8]) -> Result<Request, (String, WireError)> {
    if line.len() > MAX_LINE {
        return Err((
            String::new(),
            invalid_request("api request line is too large"),
        ));
    }
    let text = std::str::from_utf8(line)
        .map_err(|e| (String::new(), invalid_request(e)))?
        .trim();
    let v: Value = serde_json::from_str(text).map_err(|e| (String::new(), invalid_request(e)))?;
    let Some(obj) = v.as_object() else {
        return Err((
            String::new(),
            invalid_request("invalid type: expected struct Request"),
        ));
    };
    let id = match obj.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(_) => {
            return Err((
                String::new(),
                invalid_request("invalid type: id must be a string"),
            ));
        }
        None => return Err((String::new(), invalid_request("missing field `id`"))),
    };
    let method = match obj.get("method") {
        Some(Value::String(m)) if !m.is_empty() => m.clone(),
        Some(_) => {
            return Err((id, invalid_request("invalid type: method must be a string")));
        }
        None => return Err((id, invalid_request("missing field `method`"))),
    };
    let params = match obj.get("params") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(p @ Value::Object(_)) => p.clone(),
        Some(_) => {
            return Err((
                id,
                invalid_request(format!(
                    "invalid type: params of `{method}` must be an object"
                )),
            ));
        }
    };
    let token = obj
        .get("vibeke_token")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(Request {
        id,
        method,
        params,
        token,
    })
}

/// A success line: `result` must already contain `type`.
pub fn ok_line(id: &str, result: Value) -> String {
    json!({"id": id, "result": result}).to_string()
}

pub fn err_line(id: &str, e: &WireError) -> String {
    json!({"id": id, "error": {"code": e.code, "message": e.message}}).to_string()
}

pub fn event_line(dotted: &str, data: Value) -> String {
    json!({"event": super::events::wire_name(dotted), "data": data}).to_string()
}

/// `{"type": t, …fields}`.
pub fn typed(t: &str, fields: Value) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), Value::String(t.into()));
    if let Value::Object(f) = fields {
        m.extend(f);
    }
    Value::Object(m)
}

/// Map a native Vibeke error (`data.kind`, `details.object`) onto a Herdr error code.
pub fn from_native(kind: &str, object: Option<&str>, message: &str) -> WireError {
    let code = match (kind, object) {
        ("not_found", Some("pane")) => "pane_not_found",
        ("not_found", Some("tab")) => "tab_not_found",
        ("not_found", Some("workspace")) => "workspace_not_found",
        ("not_found", Some("run")) => "agent_not_found",
        ("not_found", Some("plugin")) => "plugin_not_found",
        ("not_found", Some("worktree")) => "worktree_not_found",
        ("not_found", Some("repo")) => "not_a_repository",
        ("not_found", _) => "not_found",
        ("invalid_params", _) => "invalid_params",
        ("invalid_key", _) => "invalid_key",
        ("permission_denied", _) | ("untrusted", _) => "permission_denied",
        ("timeout", _) => "timeout",
        ("conflict", _) => "conflict",
        ("method_not_found", _) => "invalid_request",
        ("unsupported", _) => "unsupported",
        _ => "internal_error",
    };
    WireError::new(code, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests() {
        let r = parse_request(br#"{"id":"1","method":"pane.list","params":{}}"#).unwrap();
        assert_eq!(r.id, "1");
        assert_eq!(r.method, "pane.list");
        let r = parse_request(b"{\"id\":\"x\",\"method\":\"ping\"}\n").unwrap();
        assert_eq!(r.params, json!({}));
        assert_eq!(r.token, None);
        let r = parse_request(br#"{"id":"1","method":"ping","vibeke_token":"abc"}"#).unwrap();
        assert_eq!(r.token.as_deref(), Some("abc"));
        // Integer ids are rejected and cannot be echoed.
        let (id, e) = parse_request(br#"{"id":1,"method":"pane.list"}"#).unwrap_err();
        assert_eq!((id.as_str(), e.code.as_str()), ("", "invalid_request"));
        // A string id is echoed on later errors.
        let (id, e) = parse_request(br#"{"id":"7","params":{}}"#).unwrap_err();
        assert_eq!((id.as_str(), e.code.as_str()), ("7", "invalid_request"));
        let (id, e) = parse_request(br#"{"id":"7","method":"x","params":[1]}"#).unwrap_err();
        assert_eq!((id.as_str(), e.code.as_str()), ("7", "invalid_request"));
        // Herdr answers every unparseable line `invalid_request` ("invalid request: …").
        let (id, e) = parse_request(b"{nope").unwrap_err();
        assert_eq!((id.as_str(), e.code.as_str()), ("", "invalid_request"));
        assert!(e.message.starts_with("invalid request: "), "{e}");
        let (_, e) = parse_request(&[0xff, 0xfe]).unwrap_err();
        assert_eq!(e.code, "invalid_request");
        let (_, e) = parse_request(b"[1]").unwrap_err();
        assert_eq!(e.code, "invalid_request");
        assert!(e.message.starts_with("invalid request: "), "{e}");
        // No answer at all for blank, non-UTF-8 and over-long lines.
        assert!(answers(br#"{"id":"1","method":"ping"}"#));
        assert!(!answers(b"  \n"));
        assert!(!answers(&[0xff, b'\n']));
        assert!(!answers(&vec![b' '; MAX_LINE + 1]));
        assert_eq!(unknown_method("nope").code, "invalid_request");
        assert_eq!(
            retired_method("pane.graphics.set").unwrap().code,
            "unknown_method"
        );
        assert!(retired_method("pane.list").is_none());
    }

    #[test]
    fn lines() {
        let ok: Value =
            serde_json::from_str(&ok_line("3", typed("pane_list", json!({"panes": []})))).unwrap();
        assert_eq!(
            ok,
            json!({"id": "3", "result": {"type": "pane_list", "panes": []}})
        );
        assert!(ok.get("jsonrpc").is_none());
        let er: Value =
            serde_json::from_str(&err_line("3", &WireError::new("pane_not_found", "w1:p9")))
                .unwrap();
        assert_eq!(
            er,
            json!({"id": "3", "error": {"code": "pane_not_found", "message": "w1:p9"}})
        );
        let ev: Value = serde_json::from_str(&event_line(
            "pane.agent_status_changed",
            json!({"pane_id": "w1:p1"}),
        ))
        .unwrap();
        assert_eq!(ev["event"], "pane_agent_status_changed");
    }

    #[test]
    fn native_errors() {
        assert_eq!(
            from_native("not_found", Some("pane"), "m").code,
            "pane_not_found"
        );
        assert_eq!(
            from_native("not_found", Some("workspace"), "m").code,
            "workspace_not_found"
        );
        assert_eq!(from_native("invalid_key", None, "m").code, "invalid_key");
        assert_eq!(
            from_native("untrusted", None, "m").code,
            "permission_denied"
        );
        assert_eq!(from_native("internal", None, "m").code, "internal_error");
    }
}
