//! JSON-RPC 2.0 envelopes and the stable error kinds (07 §1.4).

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(id: u64, method: &str, params: Value) -> Self {
        Request {
            jsonrpc: "2.0".into(),
            id: Some(Value::from(id)),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Response {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: Value, error: RpcError) -> Self {
        Response {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// Server push (no id).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: ErrorData,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorData {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub details: Value,
    #[serde(default)]
    pub retryable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    ParseError,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    NotFound,
    AmbiguousTarget,
    PermissionDenied,
    Conflict,
    Timeout,
    Stalled,
    Unsupported,
    RemoteUnavailable,
    RateLimited,
    Truncated,
    InvalidKey,
    Untrusted,
    StorageUnavailable,
    Internal,
}

impl ErrorKind {
    /// Every kind, in code order of the spec table (07 §1.4); the schema bundle lists them.
    pub const ALL: [ErrorKind; 18] = [
        ErrorKind::ParseError,
        ErrorKind::InvalidRequest,
        ErrorKind::MethodNotFound,
        ErrorKind::InvalidParams,
        ErrorKind::NotFound,
        ErrorKind::AmbiguousTarget,
        ErrorKind::PermissionDenied,
        ErrorKind::Conflict,
        ErrorKind::Timeout,
        ErrorKind::Stalled,
        ErrorKind::Unsupported,
        ErrorKind::RemoteUnavailable,
        ErrorKind::RateLimited,
        ErrorKind::Truncated,
        ErrorKind::InvalidKey,
        ErrorKind::Untrusted,
        ErrorKind::StorageUnavailable,
        ErrorKind::Internal,
    ];

    pub fn code(self) -> i64 {
        use ErrorKind::*;
        match self {
            ParseError => -32700,
            InvalidRequest => -32600,
            MethodNotFound => -32601,
            InvalidParams => -32602,
            NotFound => -32001,
            AmbiguousTarget => -32002,
            PermissionDenied => -32003,
            Conflict => -32004,
            Timeout => -32005,
            Stalled => -32006,
            Unsupported => -32007,
            RemoteUnavailable => -32008,
            RateLimited => -32009,
            Truncated => -32010,
            InvalidKey => -32011,
            Untrusted => -32012,
            StorageUnavailable => -32013,
            Internal => -32050,
        }
    }
    pub fn as_str(self) -> &'static str {
        use ErrorKind::*;
        match self {
            ParseError => "parse_error",
            InvalidRequest => "invalid_request",
            MethodNotFound => "method_not_found",
            InvalidParams => "invalid_params",
            NotFound => "not_found",
            AmbiguousTarget => "ambiguous_target",
            PermissionDenied => "permission_denied",
            Conflict => "conflict",
            Timeout => "timeout",
            Stalled => "stalled",
            Unsupported => "unsupported",
            RemoteUnavailable => "remote_unavailable",
            RateLimited => "rate_limited",
            Truncated => "truncated",
            InvalidKey => "invalid_key",
            Untrusted => "untrusted",
            StorageUnavailable => "storage_unavailable",
            Internal => "internal",
        }
    }
}

impl ErrorKind {
    /// Whether errors of this kind are marked `retryable` by [`RpcError::new`].
    pub fn retryable(self) -> bool {
        matches!(self, ErrorKind::RemoteUnavailable | ErrorKind::RateLimited)
    }
}

impl RpcError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        RpcError {
            code: kind.code(),
            message: message.into(),
            data: ErrorData {
                kind: kind.as_str().into(),
                details: Value::Null,
                retryable: kind.retryable(),
            },
        }
    }
    pub fn details(mut self, d: Value) -> Self {
        self.data.details = d;
        self
    }
    pub fn kind_is(&self, k: ErrorKind) -> bool {
        self.data.kind == k.as_str()
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.data.kind, self.message)
    }
}

impl std::error::Error for RpcError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_lists_every_kind_once() {
        let mut codes: Vec<i64> = ErrorKind::ALL.iter().map(|k| k.code()).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), ErrorKind::ALL.len());
        // An exhaustive match keeps ALL in step with the enum.
        for k in ErrorKind::ALL {
            match k {
                ErrorKind::ParseError
                | ErrorKind::InvalidRequest
                | ErrorKind::MethodNotFound
                | ErrorKind::InvalidParams
                | ErrorKind::NotFound
                | ErrorKind::AmbiguousTarget
                | ErrorKind::PermissionDenied
                | ErrorKind::Conflict
                | ErrorKind::Timeout
                | ErrorKind::Stalled
                | ErrorKind::Unsupported
                | ErrorKind::RemoteUnavailable
                | ErrorKind::RateLimited
                | ErrorKind::Truncated
                | ErrorKind::InvalidKey
                | ErrorKind::Untrusted
                | ErrorKind::StorageUnavailable
                | ErrorKind::Internal => {}
            }
        }
    }
}
