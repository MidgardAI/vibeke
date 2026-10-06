//! `preview.console_error` events (spec 06 B5): an uncaught exception or `console.error` seen
//! by an agent's headless session or a browser pane on a preview.
//!
//! Rate-limited per preview (at most [`BUDGET`] events per [`WINDOW`]); errors over the budget
//! are counted and folded into the next event that gets through as `count`. Text is redacted
//! (`vk_redact`) and clipped; the subject is `{preview, pane, task}`, so the TUI can put a `!N`
//! on the preview chip until the preview is opened.

use crate::Server;
use crate::core::Tx;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Events per preview per window.
pub const BUDGET: u32 = 5;
pub const WINDOW: Duration = Duration::from_secs(10);
/// Longest message text kept in an event.
const MAX_TEXT: usize = 300;

#[derive(Default)]
struct Slot {
    started: Option<Instant>,
    sent: u32,
    /// Errors swallowed by the limiter since the last emitted event.
    suppressed: u32,
}

/// Per-preview limiter.
#[derive(Default)]
pub struct Limiter {
    slots: Mutex<HashMap<String, Slot>>,
}

impl Limiter {
    /// `Some(count)` when an event may be sent now (`count` = this error plus the ones the
    /// limiter swallowed since the previous event); `None` when over budget.
    pub fn admit(&self, preview: &str, now: Instant) -> Option<u32> {
        let mut m = self.slots.lock().unwrap();
        if m.len() > 256 {
            m.retain(|_, s| s.started.is_some_and(|t| now.duration_since(t) < WINDOW));
        }
        let s = m.entry(preview.to_string()).or_default();
        if s.started.is_none_or(|t| now.duration_since(t) >= WINDOW) {
            s.started = Some(now);
            s.sent = 0;
        }
        if s.sent >= BUDGET {
            s.suppressed += 1;
            return None;
        }
        s.sent += 1;
        Some(1 + std::mem::take(&mut s.suppressed))
    }
}

/// What kind of problem the page reported.
pub struct ConsoleError<'a> {
    /// `exception` | `console`.
    pub source: &'a str,
    pub text: &'a str,
    pub url: Option<&'a str>,
    pub line: Option<i64>,
}

/// Emit a `preview.console_error` for `preview` (handle or id), if the limiter allows.
/// `session` is the agent session's handle when the error came from one.
pub fn report(
    server: &Server,
    preview: &str,
    pane: Option<&str>,
    session: Option<&str>,
    e: &ConsoleError<'_>,
) {
    let Some(count) = server
        .agent_browser
        .console_errors
        .admit(preview, Instant::now())
    else {
        return;
    };
    let text = vk_redact::redact(e.text);
    let text: String = text.chars().take(MAX_TEXT).collect();
    let url = e.url.map(|u| vk_redact::redact(u).into_owned());
    let mut c = server.core.lock().unwrap();
    let pv = c
        .model
        .previews
        .iter()
        .find(|p| p.id == preview || p.handle == preview)
        .cloned();
    let (handle, id, task) = match &pv {
        Some(p) => (p.handle.clone(), p.id.clone(), p.task.clone()),
        None => (preview.to_string(), preview.to_string(), None),
    };
    let task = task.or_else(|| {
        pane.and_then(|p| {
            let p = c.pane(p)?;
            c.ws(&p.workspace).and_then(|w| w.task.clone())
        })
    });
    let mut tx = Tx::new();
    tx.event(
        "preview.console_error",
        json!({"preview": handle, "preview_id": id, "pane": pane, "task": task, "machine": server.opts.machine}),
        json!({"count": count, "source": e.source, "text": text, "url": url, "line": e.line, "session": session}),
    );
    let _ = server.commit(&mut c, tx);
}

/// A CDP event from a browser pane's page (`Runtime.exceptionThrown`, `Runtime.consoleAPICalled`):
/// the minimal hook `browser_pane` calls. Looks up the pane's preview itself.
pub fn from_pane_event(server: &Server, pane: &str, method: &str, params: &Value) {
    let Some(e) = parse_event(method, params) else {
        return;
    };
    let preview = server.with_core(|c| {
        c.pane(pane)
            .and_then(|p| p.browser.as_ref())
            .and_then(|b| b.preview.clone())
    });
    let Some(preview) = preview else { return };
    report(
        server,
        &preview,
        Some(pane),
        None,
        &ConsoleError {
            source: e.0,
            text: &e.1,
            url: e.2.as_deref(),
            line: e.3,
        },
    );
}

type Parsed = (&'static str, String, Option<String>, Option<i64>);

/// `(source, text, url, line)` for the events that count as a console error.
pub fn parse_event(method: &str, p: &Value) -> Option<Parsed> {
    match method {
        "Runtime.exceptionThrown" => {
            let d = &p["exceptionDetails"];
            let text = d["exception"]["description"]
                .as_str()
                .or_else(|| d["text"].as_str())
                .unwrap_or("exception");
            Some((
                "exception",
                text.to_string(),
                d["url"].as_str().map(str::to_string),
                d["lineNumber"].as_i64(),
            ))
        }
        "Runtime.consoleAPICalled" if matches!(p["type"].as_str(), Some("error" | "assert")) => {
            let text = p["args"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|o| {
                            o.get("value")
                                .map(|v| match v {
                                    Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                })
                                .or_else(|| o["description"].as_str().map(str::to_string))
                                .unwrap_or_default()
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let f = &p["stackTrace"]["callFrames"][0];
            Some((
                "console",
                text,
                f["url"].as_str().map(str::to_string),
                f["lineNumber"].as_i64(),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_budget_window_and_coalescing() {
        let l = Limiter::default();
        let t0 = Instant::now();
        for _ in 0..BUDGET {
            assert_eq!(l.admit("v1", t0), Some(1));
        }
        // Over budget: swallowed (and counted), other previews unaffected.
        assert_eq!(l.admit("v1", t0), None);
        assert_eq!(l.admit("v1", t0 + Duration::from_secs(9)), None);
        assert_eq!(l.admit("v2", t0), Some(1));
        // Next window: the first event carries the swallowed ones.
        assert_eq!(l.admit("v1", t0 + WINDOW), Some(3));
        assert_eq!(l.admit("v1", t0 + WINDOW), Some(1));
    }

    #[test]
    fn parses_only_errors() {
        let ex = json!({"exceptionDetails": {"text": "Uncaught", "exception": {"description": "TypeError: x"}, "url": "http://a/b.js", "lineNumber": 4}});
        let (src, text, url, line) = parse_event("Runtime.exceptionThrown", &ex).unwrap();
        assert_eq!(
            (src, text.as_str(), line),
            ("exception", "TypeError: x", Some(4))
        );
        assert_eq!(url.as_deref(), Some("http://a/b.js"));
        let err = json!({"type": "error", "args": [{"value": "bad"}, {"value": 3}]});
        assert_eq!(
            parse_event("Runtime.consoleAPICalled", &err).unwrap().1,
            "bad 3"
        );
        let log = json!({"type": "log", "args": [{"value": "hi"}]});
        assert!(parse_event("Runtime.consoleAPICalled", &log).is_none());
        assert!(parse_event("Page.frameNavigated", &log).is_none());
    }
}
