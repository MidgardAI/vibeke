//! Herdr's socket wire format (07 §8.3): one newline-terminated JSON request per connection,
//! `{"id": "<string>", "method": "...", "params": {...}}`; success
//! `{"id", "result": {"type": "<result_type>", …}}`; error
//! `{"id", "error": {"code": "<snake_case>", "message"}}`; streamed events
//! `{"event": "<snake_case>", "data": {...}}`. No JSON-RPC envelope and no handshake.
//!
//! Framing limits and the exact codes for malformed input are *unverified* against the
//! baseline binary; the choices here are recorded in the inventory.

use serde_json::{Map, Value, json};

/// Longest accepted request line (bytes, without the newline).
pub const MAX_LINE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: String,
    pub method: String,
    pub params: Value,
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

/// Parse one request line. On error, returns the id to echo (the request's string id when it
/// can be recovered, `""` otherwise) with the error.
pub fn parse_request(line: &[u8]) -> Result<Request, (String, WireError)> {
    if line.len() > MAX_LINE {
        return Err((
            String::new(),
            WireError::new("invalid_request", "request line too long"),
        ));
    }
    let text = std::str::from_utf8(line).map_err(|_| {
        (
            String::new(),
            WireError::new("parse_error", "invalid UTF-8"),
        )
    })?;
    let v: Value = serde_json::from_str(text.trim_end_matches(['\r', '\n']))
        .map_err(|e| (String::new(), WireError::new("parse_error", e.to_string())))?;
    let Some(obj) = v.as_object() else {
        return Err((
            String::new(),
            WireError::new("invalid_request", "request must be a JSON object"),
        ));
    };
    let id = match obj.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(_)) => {
            return Err((
                String::new(),
                WireError::new("invalid_request", "id must be a string"),
            ));
        }
        Some(_) | None => {
            return Err((
                String::new(),
                WireError::new("invalid_request", "id must be a string"),
            ));
        }
    };
    let method = match obj.get("method") {
        Some(Value::String(m)) if !m.is_empty() => m.clone(),
        _ => {
            return Err((
                id,
                WireError::new("invalid_request", "method must be a non-empty string"),
            ));
        }
    };
    let params = match obj.get("params") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(p @ Value::Object(_)) => p.clone(),
        Some(_) => {
            return Err((
                id,
                WireError::new("invalid_params", "params must be an object"),
            ));
        }
    };
    Ok(Request { id, method, params })
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
        ("not_found", _) => "not_found",
        ("invalid_params", _) => "invalid_params",
        ("invalid_key", _) => "invalid_key",
        ("permission_denied", _) | ("untrusted", _) => "permission_denied",
        ("timeout", _) => "timeout",
        ("conflict", _) => "conflict",
        ("method_not_found", _) => "method_not_found",
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
        // Integer ids are rejected and cannot be echoed.
        let (id, e) = parse_request(br#"{"id":1,"method":"pane.list"}"#).unwrap_err();
        assert_eq!((id.as_str(), e.code.as_str()), ("", "invalid_request"));
        // A string id is echoed on later errors.
        let (id, e) = parse_request(br#"{"id":"7","params":{}}"#).unwrap_err();
        assert_eq!((id.as_str(), e.code.as_str()), ("7", "invalid_request"));
        let (id, e) = parse_request(br#"{"id":"7","method":"x","params":[1]}"#).unwrap_err();
        assert_eq!((id.as_str(), e.code.as_str()), ("7", "invalid_params"));
        let (_, e) = parse_request(b"{nope").unwrap_err();
        assert_eq!(e.code, "parse_error");
        let (_, e) = parse_request(&[0xff, 0xfe]).unwrap_err();
        assert_eq!(e.code, "parse_error");
        let (_, e) = parse_request(b"[1]").unwrap_err();
        assert_eq!(e.code, "invalid_request");
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
