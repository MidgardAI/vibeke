//! `VIBEKE_HERDR_TRACE=<file>` lines (18 §9), and the comparison with a step's expected calls.
//!
//! Vibeke writes one JSON object per line. The Herdr side writes the same shape from its socket
//! recording proxy ([`crate::herdr`]):
//!
//! ```json
//! {"ts_ms":1760000000000,"plugin":"herdr-projects","invocation":"01J…","transport":"socket",
//!  "method":"pane.split","ok":true,"code":null}
//! ```
//!
//! * `transport`: `cli` (the `herdr` shim through the broker), `socket` (a raw socket client
//!   of the broker) or `launcher` (argument parsing in `vk-cli`, before the broker).
//! * `invocation` is null outside an invocation (startup commands, panes, event hooks still
//!   carry their invocation id when they have one).
//! * `params` is present only when the scenario asks for it; it is redacted.
//! * `launcher` lines also carry `argv` (redacted) and `parse_error` when parsing failed.

use crate::scenario::ExpectedCall;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraceLine {
    pub ts_ms: u64,
    pub plugin: String,
    #[serde(default)]
    pub invocation: Option<String>,
    pub transport: String,
    pub method: String,
    pub ok: bool,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    #[serde(default)]
    pub argv: Option<Vec<String>>,
    #[serde(default)]
    pub parse_error: Option<String>,
}

/// Read a trace file; lines that are not valid trace JSON are an error (the harness owns the
/// format). Reading starts at byte `from`, so each step reads only its new lines; the second
/// value is the new offset.
pub fn read(path: &Path, from: u64) -> anyhow::Result<(Vec<TraceLine>, u64)> {
    todo!("lane MODEL")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallMismatch {
    pub expected: String,
    pub found: u32,
}

/// Check `expected` against the lines of one plugin. Unexpected extra calls are allowed; failed
/// calls that no expectation names are reported in the second list (the step decides whether
/// they matter).
pub fn match_calls(
    lines: &[TraceLine],
    plugin: &str,
    expected: &[ExpectedCall],
) -> (Vec<CallMismatch>, Vec<TraceLine>) {
    todo!("lane MODEL")
}
