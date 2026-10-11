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
///
/// A last line without its newline is still being written: reading stops before it, and the
/// offset stays at its start, so the next read sees it whole. A missing file has no lines yet.
pub fn read(path: &Path, from: u64) -> anyhow::Result<(Vec<TraceLine>, u64)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), from)),
        Err(e) => return Err(anyhow::anyhow!("{}: {e}", path.display())),
    };
    file.seek(SeekFrom::Start(from))
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let complete = match buf.iter().rposition(|&b| b == b'\n') {
        Some(i) => i + 1,
        None => 0,
    };
    let mut lines = Vec::new();
    let mut at = from;
    for raw in buf[..complete].split_inclusive(|&b| b == b'\n') {
        let line = raw.strip_suffix(b"\n").unwrap_or(raw);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !line.iter().all(u8::is_ascii_whitespace) {
            let parsed: TraceLine = serde_json::from_slice(line).map_err(|e| {
                anyhow::anyhow!("{}: bad trace line at byte {at}: {e}", path.display())
            })?;
            lines.push(parsed);
        }
        at += raw.len() as u64;
    }
    Ok((lines, from + complete as u64))
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
    let mine: Vec<&TraceLine> = lines.iter().filter(|l| l.plugin == plugin).collect();
    let names = |e: &ExpectedCall, l: &TraceLine| {
        l.method == e.method
            && l.ok == e.ok
            && (e.code.is_none() || l.code.as_deref() == e.code.as_deref())
    };
    let mut mismatches = Vec::new();
    for e in expected {
        let found = mine.iter().filter(|l| names(e, l)).count() as u32;
        let min = e.min.unwrap_or(1);
        if found < min {
            mismatches.push(CallMismatch {
                expected: describe(e, min),
                found,
            });
        }
    }
    let unexpected_failures = mine
        .into_iter()
        .filter(|l| !l.ok && !expected.iter().any(|e| names(e, l)))
        .cloned()
        .collect();
    (mismatches, unexpected_failures)
}

/// `pane.split ok`, `popup.open failed not_found`, with ` ×N` when more than one is needed.
fn describe(e: &ExpectedCall, min: u32) -> String {
    let mut s = e.method.clone();
    s.push_str(if e.ok { " ok" } else { " failed" });
    if let Some(c) = &e.code {
        s.push(' ');
        s.push_str(c);
    }
    if min > 1 {
        s.push_str(&format!(" ×{min}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(plugin: &str, method: &str, ok: bool, code: Option<&str>) -> String {
        serde_json::json!({
            "ts_ms": 1_760_000_000_000u64,
            "plugin": plugin,
            "invocation": null,
            "transport": "socket",
            "method": method,
            "ok": ok,
            "code": code,
        })
        .to_string()
    }

    fn call(method: &str, ok: bool, code: Option<&str>, min: Option<u32>) -> ExpectedCall {
        ExpectedCall {
            method: method.into(),
            ok,
            code: code.map(str::to_string),
            min,
        }
    }

    #[test]
    fn reads_new_lines_and_waits_for_partial_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let (lines, off) = read(&path, 0).unwrap();
        assert!(lines.is_empty());
        assert_eq!(off, 0);

        let first = line("p", "pane.split", true, None);
        let second = line("p", "popup.open", false, Some("not_found"));
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "{first}\n{}", &second[..10]).unwrap();
        f.flush().unwrap();
        let (lines, off) = read(&path, 0).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].method, "pane.split");
        assert_eq!(off, first.len() as u64 + 1);

        // The partial line is not consumed; it is read whole once it is finished.
        let (lines, again) = read(&path, off).unwrap();
        assert!(lines.is_empty());
        assert_eq!(again, off);
        write!(f, "{}\n\n", &second[10..]).unwrap();
        f.flush().unwrap();
        let (lines, end) = read(&path, off).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].code.as_deref(), Some("not_found"));
        assert_eq!(end, std::fs::metadata(&path).unwrap().len());
        assert!(read(&path, end).unwrap().0.is_empty());
    }

    #[test]
    fn bad_lines_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        std::fs::write(&path, format!("{}\nnot json\n", line("p", "x", true, None))).unwrap();
        let e = read(&path, 0).unwrap_err().to_string();
        assert!(e.contains("bad trace line"), "{e}");
    }

    fn parsed(raw: &[String]) -> Vec<TraceLine> {
        raw.iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn matches_expected_calls() {
        let lines = parsed(&[
            line("p", "pane.split", true, None),
            line("p", "pane.split", true, None),
            line("p", "popup.open", false, Some("not_found")),
            line("p", "pane.get", false, Some("forbidden")),
            line("other", "tab.create", true, None),
            line("other", "tab.close", false, Some("x")),
        ]);
        let (miss, fails) = match_calls(
            &lines,
            "p",
            &[
                call("pane.split", true, None, Some(2)),
                call("popup.open", false, Some("not_found"), None),
            ],
        );
        assert!(miss.is_empty(), "{miss:?}");
        assert_eq!(fails.len(), 1, "only the unnamed failure of this plugin");
        assert_eq!(fails[0].method, "pane.get");

        let (miss, _) = match_calls(
            &lines,
            "p",
            &[
                call("pane.split", true, None, Some(3)),
                call("popup.open", true, None, None),
                call("popup.open", false, Some("other"), None),
                call("tab.create", true, None, None),
            ],
        );
        assert_eq!(
            miss,
            vec![
                CallMismatch {
                    expected: "pane.split ok ×3".into(),
                    found: 2
                },
                CallMismatch {
                    expected: "popup.open ok".into(),
                    found: 0
                },
                CallMismatch {
                    expected: "popup.open failed other".into(),
                    found: 0
                },
                CallMismatch {
                    expected: "tab.create ok".into(),
                    found: 0
                },
            ]
        );

        // A failed expectation without a code names any failure of that method.
        let (miss, fails) = match_calls(&lines, "p", &[call("pane.get", false, None, None)]);
        assert!(miss.is_empty());
        assert_eq!(fails.len(), 1);
        assert_eq!(fails[0].method, "popup.open");
    }
}
