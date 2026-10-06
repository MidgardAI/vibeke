//! Console and network capture from CDP events (06 B5 "Console/network capture", reused by the
//! browser pane's console split, 06 B3.2).
//!
//! The entry shapes are the agent browser's (`browser.console` / `browser.network`):
//! console `{ts, level, text, source, url, line}`, network `{ts, method, url, type, status,
//! mime, error, blocked_reason, duration_ms}`. [`Capture`] keeps both rings with one sequence
//! counter (so a follower can ask for "everything after N" across both) and the in-flight
//! request table. Callers redact before storing anything that leaves the process.

use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};

/// Entries kept per ring (06 B5).
pub const RING: usize = 500;
/// In-flight requests tracked at most (a page that never finishes requests can't grow it).
const INFLIGHT_MAX: usize = 2000;

/// Which ring an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Console,
    Network,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Console => "console",
            Kind::Network => "network",
        }
    }
}

/// Normalise a `console.*` type or a log level.
pub fn console_level(t: &str) -> &'static str {
    match t {
        "error" | "assert" => "error",
        "warning" | "warn" => "warn",
        "debug" | "verbose" => "debug",
        "info" => "info",
        _ => "log",
    }
}

/// Text of a `Runtime.RemoteObject` argument.
pub fn remote_object_text(o: &Value) -> String {
    if let Some(v) = o.get("value") {
        return match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    o.get("description")
        .or_else(|| o.get("unserializableValue"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| o["type"].as_str().unwrap_or(""))
        .to_string()
}

/// Is this entry an error (console level error, or a failed / blocked / ≥ 400 request)?
pub fn is_error(kind: Kind, e: &Value) -> bool {
    match kind {
        Kind::Console => e["level"] == "error",
        Kind::Network => {
            !e["error"].is_null()
                || !e["blocked_by_policy"].is_null()
                || e["status"].as_u64().is_some_and(|s| s >= 400)
        }
    }
}

/// Console and network rings of one page.
#[derive(Debug, Default)]
pub struct Capture {
    pub console: VecDeque<Value>,
    pub network: VecDeque<Value>,
    inflight: HashMap<String, Value>,
    /// Last sequence number handed out (entries carry `seq`).
    pub seq: u64,
}

impl Capture {
    /// Store an entry (assigning `seq` and `kind`); returns it as stored.
    pub fn push(&mut self, kind: Kind, mut e: Value) -> Value {
        self.seq += 1;
        e["seq"] = json!(self.seq);
        e["kind"] = json!(kind.as_str());
        let ring = match kind {
            Kind::Console => &mut self.console,
            Kind::Network => &mut self.network,
        };
        if ring.len() >= RING {
            ring.pop_front();
        }
        ring.push_back(e.clone());
        e
    }

    /// Feed one CDP event. Returns the finished entry (console line, or a completed/failed
    /// request) when the event produced one; `redact` is applied to its text and URL first.
    pub fn on_event(
        &mut self,
        method: &str,
        p: &Value,
        now_ms: i64,
        redact: &dyn Fn(&str) -> String,
    ) -> Option<(Kind, Value)> {
        let (kind, mut e) = match method {
            "Runtime.consoleAPICalled" => {
                let text = p["args"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(remote_object_text)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let frame = &p["stackTrace"]["callFrames"][0];
                (
                    Kind::Console,
                    json!({"ts": now_ms, "level": console_level(p["type"].as_str().unwrap_or("log")), "text": text,
                           "source": "console", "url": frame["url"], "line": frame["lineNumber"]}),
                )
            }
            "Runtime.exceptionThrown" => {
                let d = &p["exceptionDetails"];
                let text = d["exception"]["description"]
                    .as_str()
                    .or_else(|| d["text"].as_str())
                    .unwrap_or("exception")
                    .to_string();
                (
                    Kind::Console,
                    json!({"ts": now_ms, "level": "error", "text": text, "source": "exception",
                           "url": d["url"], "line": d["lineNumber"]}),
                )
            }
            "Log.entryAdded" => {
                let e = &p["entry"];
                (
                    Kind::Console,
                    json!({"ts": now_ms, "level": console_level(e["level"].as_str().unwrap_or("info")), "text": e["text"],
                           "source": e["source"], "url": e["url"], "line": e["lineNumber"]}),
                )
            }
            "Network.requestWillBeSent" => {
                let id = p["requestId"].as_str().unwrap_or("").to_string();
                if self.inflight.len() >= INFLIGHT_MAX {
                    self.inflight.clear();
                }
                // A redirect reuses the request id: the previous hop is done.
                let prev = self.inflight.insert(
                    id,
                    json!({"ts": now_ms, "method": p["request"]["method"], "url": p["request"]["url"],
                           "type": p["type"], "status": null, "error": null}),
                );
                let mut prev = prev?;
                if p["redirectResponse"].is_object() {
                    prev["status"] = p["redirectResponse"]["status"].clone();
                }
                prev["duration_ms"] = json!(now_ms - prev["ts"].as_i64().unwrap_or(now_ms));
                (Kind::Network, prev)
            }
            "Network.responseReceived" => {
                let id = p["requestId"].as_str().unwrap_or("");
                if let Some(e) = self.inflight.get_mut(id) {
                    e["status"] = p["response"]["status"].clone();
                    e["mime"] = p["response"]["mimeType"].clone();
                }
                return None;
            }
            "Network.loadingFinished" | "Network.loadingFailed" => {
                let id = p["requestId"].as_str().unwrap_or("");
                let mut e = self
                    .inflight
                    .remove(id)
                    .unwrap_or_else(|| json!({"ts": now_ms, "type": p["type"]}));
                if method == "Network.loadingFailed" {
                    e["error"] = p["errorText"].clone();
                    if p["blockedReason"].is_string() {
                        e["blocked_reason"] = p["blockedReason"].clone();
                    }
                    if p["canceled"] == true {
                        e["canceled"] = json!(true);
                    }
                }
                e["duration_ms"] = json!(now_ms - e["ts"].as_i64().unwrap_or(now_ms));
                (Kind::Network, e)
            }
            _ => return None,
        };
        for k in ["text", "url"] {
            if let Some(s) = e[k].as_str() {
                let r = redact(s);
                e[k] = json!(r);
            }
        }
        Some((kind, e))
    }

    /// Entries after `after` (by `seq`), oldest first, filtered, at most `limit` (the newest).
    pub fn query(&self, f: &Filter) -> Vec<Value> {
        let mut v: Vec<&Value> = Vec::new();
        if f.console {
            v.extend(self.console.iter());
        }
        if f.network {
            v.extend(self.network.iter());
        }
        let mut out: Vec<Value> = v.into_iter().filter(|e| f.matches(e)).cloned().collect();
        out.sort_by_key(|e| e["seq"].as_u64().unwrap_or(0));
        let skip = out.len().saturating_sub(f.limit.max(1));
        out.drain(..skip);
        out
    }

    /// Forget everything (a new document in a page that asked for it, tests).
    pub fn clear(&mut self) {
        self.console.clear();
        self.network.clear();
        self.inflight.clear();
    }
}

/// What a follower wants (`c` console, `n` network, `e` errors only).
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub console: bool,
    pub network: bool,
    pub errors: bool,
    /// Only entries with `seq` > this.
    pub after: u64,
    /// Only entries at or after this time (ms since epoch).
    pub since_ms: Option<i64>,
    pub limit: usize,
}

impl Default for Filter {
    fn default() -> Self {
        Filter {
            console: true,
            network: true,
            errors: false,
            after: 0,
            since_ms: None,
            limit: 200,
        }
    }
}

impl Filter {
    pub fn matches(&self, e: &Value) -> bool {
        let kind = if e["kind"] == "network" {
            Kind::Network
        } else {
            Kind::Console
        };
        let on = match kind {
            Kind::Console => self.console,
            Kind::Network => self.network,
        };
        on && e["seq"].as_u64().unwrap_or(0) > self.after
            && self
                .since_ms
                .is_none_or(|t| e["ts"].as_i64().unwrap_or(0) >= t)
            && (!self.errors || is_error(kind, e))
    }
}

/// One line of text for a terminal follower: `12:03:04.120 error  text  (url:line)` /
/// `12:03:04.120 GET 404  url  (fetch, 12 ms)`. Times are local wall-clock.
pub fn format_line(e: &Value, local_offset_s: i64) -> String {
    let ts = e["ts"].as_i64().unwrap_or(0) + local_offset_s * 1000;
    let day_ms = ts.rem_euclid(86_400_000);
    let t = format!(
        "{:02}:{:02}:{:02}.{:03}",
        day_ms / 3_600_000,
        day_ms / 60_000 % 60,
        day_ms / 1000 % 60,
        day_ms % 1000
    );
    let one_line = |s: &str| s.replace(['\n', '\r'], " ⏎ ");
    if e["kind"] == "network" {
        let method = e["method"].as_str().unwrap_or("GET");
        let status = match (&e["error"], e["status"].as_u64()) {
            (Value::String(err), _) => format!("✗ {err}"),
            (_, Some(s)) => s.to_string(),
            _ => "…".into(),
        };
        let mut tail = Vec::new();
        if let Some(t) = e["type"].as_str() {
            tail.push(t.to_ascii_lowercase());
        }
        if let Some(ms) = e["duration_ms"].as_i64() {
            tail.push(format!("{ms} ms"));
        }
        if let Some(r) = e["blocked_reason"].as_str() {
            tail.push(format!("blocked: {r}"));
        }
        format!(
            "{t} {method} {status}  {}  ({})",
            one_line(e["url"].as_str().unwrap_or("")),
            tail.join(", ")
        )
    } else {
        let level = e["level"].as_str().unwrap_or("log");
        let loc = match (
            e["url"].as_str().filter(|u| !u.is_empty()),
            e["line"].as_i64(),
        ) {
            (Some(u), Some(l)) => format!("  ({}:{})", u.rsplit('/').next().unwrap_or(u), l + 1),
            (Some(u), None) => format!("  ({})", u.rsplit('/').next().unwrap_or(u)),
            _ => String::new(),
        };
        format!(
            "{t} {level:<5}  {}{loc}",
            one_line(e["text"].as_str().unwrap_or(""))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_network_and_errors() {
        let mut c = Capture::default();
        let red = |s: &str| s.replace("hunter2", "[REDACTED]");
        let ev = |c: &mut Capture, m: &str, p: Value, t: i64| {
            if let Some((k, e)) = c.on_event(m, &p, t, &red) {
                c.push(k, e);
            }
        };
        ev(
            &mut c,
            "Runtime.consoleAPICalled",
            json!({"type": "log", "args": [{"type": "string", "value": "hello"}, {"type": "number", "value": 3}],
                   "stackTrace": {"callFrames": [{"url": "http://localhost:5173/app.js", "lineNumber": 11}]}}),
            1_000,
        );
        ev(
            &mut c,
            "Runtime.exceptionThrown",
            json!({"exceptionDetails": {"text": "Uncaught", "exception": {"description": "TypeError: x is undefined"}}}),
            1_001,
        );
        ev(
            &mut c,
            "Log.entryAdded",
            json!({"entry": {"level": "warning", "text": "token=hunter2 deprecated", "source": "other"}}),
            1_002,
        );
        ev(
            &mut c,
            "Network.requestWillBeSent",
            json!({"requestId": "1", "type": "Fetch", "request": {"method": "GET", "url": "http://localhost:5173/api?pw=hunter2"}}),
            1_003,
        );
        ev(
            &mut c,
            "Network.responseReceived",
            json!({"requestId": "1", "response": {"status": 404, "mimeType": "application/json"}}),
            1_010,
        );
        ev(
            &mut c,
            "Network.loadingFinished",
            json!({"requestId": "1"}),
            1_015,
        );
        ev(
            &mut c,
            "Network.requestWillBeSent",
            json!({"requestId": "2", "type": "Script", "request": {"method": "GET", "url": "http://localhost:5173/ok.js"}}),
            1_020,
        );
        ev(
            &mut c,
            "Network.responseReceived",
            json!({"requestId": "2", "response": {"status": 200}}),
            1_021,
        );
        ev(
            &mut c,
            "Network.loadingFinished",
            json!({"requestId": "2"}),
            1_022,
        );
        ev(
            &mut c,
            "Network.requestWillBeSent",
            json!({"requestId": "3", "type": "XHR", "request": {"method": "POST", "url": "http://localhost:9/x"}}),
            1_030,
        );
        ev(
            &mut c,
            "Network.loadingFailed",
            json!({"requestId": "3", "errorText": "net::ERR_CONNECTION_REFUSED"}),
            1_031,
        );
        assert_eq!(c.console.len(), 3);
        assert_eq!(c.network.len(), 3);
        assert_eq!(c.console[0]["text"], "hello 3");
        assert_eq!(c.console[0]["line"], 11);
        assert_eq!(c.console[1]["level"], "error");
        assert_eq!(c.console[2]["text"], "token=[REDACTED] deprecated");
        assert_eq!(
            c.network[0]["url"],
            "http://localhost:5173/api?pw=[REDACTED]"
        );
        assert_eq!(c.network[0]["status"], 404);
        assert_eq!(c.network[0]["duration_ms"], 12);
        // Everything in seq order across both rings.
        let all = c.query(&Filter::default());
        let seqs: Vec<u64> = all.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
        assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6]);
        // c / n / e filters.
        let only = |console, network, errors| {
            c.query(&Filter {
                console,
                network,
                errors,
                ..Default::default()
            })
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect::<Vec<_>>()
        };
        assert_eq!(only(true, false, false), vec![1, 2, 3]);
        assert_eq!(only(false, true, false), vec![4, 5, 6]);
        assert_eq!(only(true, true, true), vec![2, 4, 6]);
        // after / limit.
        let f = Filter {
            after: 4,
            ..Default::default()
        };
        assert_eq!(c.query(&f).len(), 2);
        let f = Filter {
            limit: 1,
            ..Default::default()
        };
        assert_eq!(c.query(&f)[0]["seq"], 6);
        // Formatting.
        let line = format_line(&all[0], 0);
        assert!(line.starts_with("00:00:01.000 log"), "{line}");
        assert!(line.ends_with("(app.js:12)"), "{line}");
        let line = format_line(&all[3], 0);
        assert!(
            line.contains("GET 404  http://localhost:5173/api"),
            "{line}"
        );
        assert!(line.contains("(fetch, 12 ms)"), "{line}");
        assert!(format_line(&all[5], 0).contains("POST ✗ net::ERR_CONNECTION_REFUSED"));
    }

    #[test]
    fn rings_are_bounded_and_redirects_complete_the_previous_hop() {
        let mut c = Capture::default();
        for i in 0..(RING + 10) {
            c.push(Kind::Console, json!({"ts": i, "level": "log", "text": "x"}));
        }
        assert_eq!(c.console.len(), RING);
        assert_eq!(c.console[0]["seq"], 11);
        let red = |s: &str| s.to_string();
        assert!(
            c.on_event(
                "Network.requestWillBeSent",
                &json!({"requestId": "r", "request": {"method": "GET", "url": "http://a/"}}),
                0,
                &red
            )
            .is_none()
        );
        let (k, e) = c
            .on_event(
                "Network.requestWillBeSent",
                &json!({"requestId": "r", "request": {"method": "GET", "url": "http://a/next"},
                        "redirectResponse": {"status": 302}}),
                5,
                &red,
            )
            .unwrap();
        assert_eq!(
            (k, e["status"].as_u64(), e["url"].as_str()),
            (Kind::Network, Some(302), Some("http://a/"))
        );
    }
}
