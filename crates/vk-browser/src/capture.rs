//! Console and network capture from CDP events (06 B5 "Console/network capture", reused by the
//! browser pane's console split, 06 B3.2).
//!
//! The entry shapes are the agent browser's (`browser.console` / `browser.network`):
//! console `{ts, level, text, source, url, line}`, network `{ts, method, url, type, status,
//! mime, error, blocked_reason, duration_ms}`. [`Capture`] keeps both rings with one sequence
//! counter (so a follower can ask for "everything after N" across both) and the in-flight
//! request table. Callers redact before storing anything that leaves the process.
//!
//! Every entry is tagged with where it came from: the `origin` and `document` URL of the frame
//! that produced it (its execution context, the request's document, or a log entry's URL),
//! and [`Captured::nav`], the top-level navigation that frame or request belonged to. The
//! browser pane decides from that, per entry and at capture time, what may be relayed to a
//! remote owner (06 B3.2). Page text is terminal-safe only after [`escape_controls`] (applied
//! by [`format_line`], and by the server before it serves entries).

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

/// Control characters (C0 except newline and tab, DEL, C1) as visible `\x1b`-style escapes,
/// so captured page text can't drive a terminal (OSC 52, titles, notifications, cursor moves).
pub fn escape_controls(s: &str) -> String {
    escape_with(s, &['\n', '\t'])
}

fn escape_with(s: &str, keep: &[char]) -> String {
    if !s.chars().any(|c| is_control(c) && !keep.contains(&c)) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if is_control(c) && !keep.contains(&c) {
            out.push_str(&format!("\\x{:02x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

fn is_control(c: char) -> bool {
    matches!(c as u32, 0x00..=0x1f | 0x7f..=0x9f)
}

/// Every string in `v` (recursively) through [`escape_controls`].
pub fn escape_value(v: &mut Value) {
    match v {
        Value::String(s) => {
            if s.chars().any(is_control) {
                *s = escape_controls(s);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(escape_value),
        Value::Object(o) => o.values_mut().for_each(escape_value),
        _ => {}
    }
}

/// Origin (`scheme://host[:port]`) of an http(s)/ws(s) URL; `""` when it has none (`about:`,
/// `data:`, `file:`, junk).
pub fn origin_of(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return String::new();
    };
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https" | "ws" | "wss") {
        return String::new();
    }
    let auth = rest.split(['/', '?', '#', '\\']).next().unwrap_or("");
    // Credentials are not part of an origin.
    let host = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    if host.is_empty() {
        return String::new();
    }
    format!("{scheme}://{}", host.to_ascii_lowercase())
}

/// An execution context (`Runtime.executionContextCreated`).
#[derive(Debug, Clone)]
struct ExecCtx {
    origin: String,
    frame: String,
    default: bool,
    /// The top-level navigation it was created in.
    nav: u64,
}

/// An entry [`Capture::on_event`] produced, with its source.
#[derive(Debug, Clone)]
pub struct Captured {
    pub kind: Kind,
    /// The entry (redacted text and URLs, tagged with `origin` and `document`).
    pub entry: Value,
    /// Origin of the frame that produced it (`""` when unknown).
    pub origin: String,
    /// The top-level navigation its frame or request belonged to ([`Capture::nav`] then).
    pub nav: u64,
}

/// Console and network rings of one page.
#[derive(Debug, Default)]
pub struct Capture {
    pub console: VecDeque<Value>,
    pub network: VecDeque<Value>,
    inflight: HashMap<String, Value>,
    /// Last sequence number handed out (entries carry `seq`).
    pub seq: u64,
    /// Top-level navigations seen (bumped on every main-frame `Page.frameNavigated`).
    pub nav: u64,
    /// URL of the current top-level document (`""` before the first navigation).
    pub top_url: String,
    main_frame: Option<String>,
    contexts: HashMap<i64, ExecCtx>,
    /// Frame id → its document URL.
    frames: HashMap<String, String>,
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

    /// Track frames, documents and execution contexts (`Page.frameNavigated`,
    /// `Runtime.executionContext*`). Returns true for a top-level navigation.
    pub fn on_page_event(&mut self, method: &str, p: &Value) -> bool {
        match method {
            "Page.frameNavigated" => {
                let f = &p["frame"];
                let id = f["id"].as_str().unwrap_or("").to_string();
                let url = f["url"].as_str().unwrap_or("").to_string();
                if self.frames.len() >= INFLIGHT_MAX {
                    self.frames.clear();
                }
                self.frames.insert(id.clone(), url.clone());
                if f.get("parentId").is_some_and(|x| !x.is_null()) {
                    return false;
                }
                self.nav += 1;
                self.main_frame = Some(id);
                self.top_url = url;
                // Requests in flight belong to the previous document (their `nav` no longer
                // matches), except the one that loaded this document (same loader).
                let (nav, loader) = (self.nav, f["loaderId"].as_str().unwrap_or(""));
                for e in self.inflight.values_mut() {
                    if !loader.is_empty() && e["_loader"] == loader {
                        e["_nav"] = json!(nav);
                    }
                }
                true
            }
            "Runtime.executionContextCreated" => {
                let c = &p["context"];
                if let Some(id) = c["id"].as_i64() {
                    if self.contexts.len() >= INFLIGHT_MAX {
                        self.contexts.clear();
                    }
                    self.contexts.insert(
                        id,
                        ExecCtx {
                            origin: c["origin"].as_str().unwrap_or("").to_string(),
                            frame: c["auxData"]["frameId"].as_str().unwrap_or("").to_string(),
                            default: c["auxData"]["isDefault"].as_bool().unwrap_or(false),
                            nav: self.nav,
                        },
                    );
                }
                false
            }
            "Runtime.executionContextDestroyed" => {
                if let Some(id) = p["executionContextId"].as_i64() {
                    self.contexts.remove(&id);
                }
                false
            }
            "Runtime.executionContextsCleared" => {
                self.contexts.clear();
                false
            }
            _ => false,
        }
    }

    /// Is `ctx` the main world of the current top-level document (not an iframe, not an
    /// isolated world, not a context left from an earlier navigation)?
    pub fn is_top_main_world(&self, ctx: i64) -> bool {
        self.contexts.get(&ctx).is_some_and(|c| {
            c.default && c.nav == self.nav && self.main_frame.as_deref() == Some(c.frame.as_str())
        })
    }

    /// Origin, document URL and navigation of execution context `ctx` (an unknown context has
    /// no origin, so its entries are never relayed).
    fn ctx_source(&self, ctx: Option<i64>) -> (String, String, u64) {
        match ctx.and_then(|c| self.contexts.get(&c)) {
            Some(c) => (
                c.origin.clone(),
                self.frames.get(&c.frame).cloned().unwrap_or_default(),
                c.nav,
            ),
            None => (String::new(), String::new(), 0),
        }
    }

    /// Feed one CDP event. Returns the finished entry (console line, or a completed/failed
    /// request) when the event produced one; `redact` is applied to its text and URLs first.
    pub fn on_event(
        &mut self,
        method: &str,
        p: &Value,
        now_ms: i64,
        redact: &dyn Fn(&str) -> String,
    ) -> Option<Captured> {
        let (kind, mut e, (origin, document, nav)) = match method {
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
                    self.ctx_source(p["executionContextId"].as_i64()),
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
                    self.ctx_source(d["executionContextId"].as_i64()),
                )
            }
            "Log.entryAdded" => {
                let e = &p["entry"];
                // A log entry names no frame: it is attributed to the URL it names, if any.
                let url = e["url"].as_str().unwrap_or("");
                (
                    Kind::Console,
                    json!({"ts": now_ms, "level": console_level(e["level"].as_str().unwrap_or("info")), "text": e["text"],
                           "source": e["source"], "url": e["url"], "line": e["lineNumber"]}),
                    (origin_of(url), String::new(), self.nav),
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
                           "type": p["type"], "status": null, "error": null,
                           "_doc": p["documentURL"], "_loader": p["loaderId"], "_nav": self.nav}),
                );
                let mut prev = prev?;
                if p["redirectResponse"].is_object() {
                    prev["status"] = p["redirectResponse"]["status"].clone();
                }
                prev["duration_ms"] = json!(now_ms - prev["ts"].as_i64().unwrap_or(now_ms));
                let src = request_source(&mut prev);
                (Kind::Network, prev, src)
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
                let src = request_source(&mut e);
                (Kind::Network, e, src)
            }
            _ => return None,
        };
        e["origin"] = json!(origin);
        e["document"] = if document.is_empty() {
            Value::Null
        } else {
            json!(document)
        };
        for k in ["text", "url", "origin", "document"] {
            if let Some(s) = e[k].as_str() {
                let r = redact(s);
                e[k] = json!(r);
            }
        }
        Some(Captured {
            kind,
            entry: e,
            origin,
            nav,
        })
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

/// Source of a finished request, with its internal tags removed: the requesting document's
/// origin and URL (`documentURL`) and the navigation it started in. A request that finished
/// without a recorded start has no source (never relayed).
fn request_source(e: &mut Value) -> (String, String, u64) {
    let doc = e["_doc"].as_str().unwrap_or("").to_string();
    let nav = e["_nav"].as_u64().unwrap_or(0);
    if let Some(o) = e.as_object_mut() {
        o.remove("_doc");
        o.remove("_loader");
        o.remove("_nav");
    }
    (origin_of(&doc), doc, nav)
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
    // One line, and nothing a terminal would act on: every field is page-controlled.
    let one_line = |s: &str| escape_with(&s.replace(['\n', '\r'], " ⏎ ").replace('\t', " "), &[]);
    if e["kind"] == "network" {
        let method = one_line(e["method"].as_str().unwrap_or("GET"));
        let status = match (&e["error"], e["status"].as_u64()) {
            (Value::String(err), _) => format!("✗ {}", one_line(err)),
            (_, Some(s)) => s.to_string(),
            _ => "…".into(),
        };
        let mut tail = Vec::new();
        if let Some(t) = e["type"].as_str() {
            tail.push(one_line(&t.to_ascii_lowercase()));
        }
        if let Some(ms) = e["duration_ms"].as_i64() {
            tail.push(format!("{ms} ms"));
        }
        if let Some(r) = e["blocked_reason"].as_str() {
            tail.push(format!("blocked: {}", one_line(r)));
        }
        format!(
            "{t} {method} {status}  {}  ({})",
            one_line(e["url"].as_str().unwrap_or("")),
            tail.join(", ")
        )
    } else {
        let level = one_line(e["level"].as_str().unwrap_or("log"));
        let loc = match (
            e["url"].as_str().filter(|u| !u.is_empty()),
            e["line"].as_i64(),
        ) {
            (Some(u), Some(l)) => {
                format!(
                    "  ({}:{})",
                    one_line(u.rsplit('/').next().unwrap_or(u)),
                    l + 1
                )
            }
            (Some(u), None) => format!("  ({})", one_line(u.rsplit('/').next().unwrap_or(u))),
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
            if let Some(x) = c.on_event(m, &p, t, &red) {
                c.push(x.kind, x.entry);
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
        let x = c
            .on_event(
                "Network.requestWillBeSent",
                &json!({"requestId": "r", "request": {"method": "GET", "url": "http://a/next"},
                        "redirectResponse": {"status": 302}}),
                5,
                &red,
            )
            .unwrap();
        let (k, e) = (x.kind, x.entry);
        assert_eq!(
            (k, e["status"].as_u64(), e["url"].as_str()),
            (Kind::Network, Some(302), Some("http://a/"))
        );
        assert!(e.get("_nav").is_none() && e.get("_doc").is_none(), "{e}");
    }

    const HOSTILE: &str = "pwned \x1b]52;c;YXR0YWNrZXI=\x07 \x1b]0;title\x07 \x1b]9;notify\x07 \x1b[2J\x1b[H \u{9b}31m \x7f end";

    #[test]
    fn control_characters_are_escaped_visibly() {
        assert_eq!(
            escape_controls("a\x1bb\x07c\u{9b}d\x7fe\n\tf"),
            "a\\x1bb\\x07c\\x9bd\\x7fe\n\tf"
        );
        assert_eq!(escape_controls("plain ⏎ text"), "plain ⏎ text");
        let mut v = json!({"text": "x\x1b]52;c;QQ==\x07", "n": 1, "a": ["\r"]});
        escape_value(&mut v);
        assert_eq!(
            v,
            json!({"text": "x\\x1b]52;c;QQ==\\x07", "n": 1, "a": ["\\x0d"]})
        );
        // Idempotent: escaped text has nothing left to escape.
        let once = escape_controls(HOSTILE);
        assert_eq!(escape_controls(&once), once);
    }

    /// Hostile console text through the follower's formatting and a terminal engine: shown as
    /// visible escapes, and no clipboard write, notification, title change or other effect.
    #[test]
    fn hostile_console_text_is_inert_in_a_terminal() {
        for e in [
            json!({"kind": "console", "ts": 0, "level": "log", "text": HOSTILE, "url": format!("http://x/{HOSTILE}"), "line": 1, "seq": 1}),
            json!({"kind": "network", "ts": 0, "method": HOSTILE, "url": HOSTILE, "type": HOSTILE,
                   "error": HOSTILE, "blocked_reason": HOSTILE, "duration_ms": 1, "seq": 2}),
        ] {
            let line = format_line(&e, 0);
            assert!(!line.chars().any(is_control), "{line:?}");
            let mut engine = vk_term::Engine::new(200, 10, 100);
            let mut fx = Vec::new();
            engine.feed(format!("{line}\r\n").as_bytes(), &mut fx);
            assert!(fx.is_empty(), "terminal effects: {fx:?}");
            assert_eq!(engine.title(), "");
            let screen = engine.screen_text();
            assert!(screen.contains("\\x1b]52;c;YXR0YWNrZXI=\\x07"), "{screen}");
        }
    }

    #[test]
    fn entries_carry_their_frame_origin_and_navigation() {
        let red = |s: &str| s.to_string();
        let mut c = Capture::default();
        let nav = |c: &mut Capture, url: &str, loader: &str| {
            assert!(c.on_page_event(
                "Page.frameNavigated",
                &json!({"frame": {"id": "M", "url": url, "loaderId": loader}})
            ));
        };
        let ctx = |c: &mut Capture, id: i64, origin: &str, frame: &str, default: bool| {
            c.on_page_event(
                "Runtime.executionContextCreated",
                &json!({"context": {"id": id, "origin": origin, "auxData": {"isDefault": default, "frameId": frame}}}),
            );
        };
        let log = |c: &mut Capture, ctx: i64| {
            c.on_event(
                "Runtime.consoleAPICalled",
                &json!({"type": "log", "executionContextId": ctx, "args": [{"type": "string", "value": "x"}]}),
                0,
                &red,
            )
            .unwrap()
        };
        nav(&mut c, "http://localhost:5173/", "L1");
        ctx(&mut c, 1, "http://localhost:5173", "M", true);
        assert!(!c.on_page_event(
            "Page.frameNavigated",
            &json!({"frame": {"id": "I", "parentId": "M", "url": "https://ads.example/f", "loaderId": "LI"}})
        ));
        ctx(&mut c, 2, "https://ads.example", "I", true);
        ctx(&mut c, 3, "http://localhost:5173", "M", false); // an isolated world
        let top = log(&mut c, 1);
        assert_eq!((top.origin.as_str(), top.nav), ("http://localhost:5173", 1));
        assert_eq!(top.entry["origin"], "http://localhost:5173");
        assert_eq!(top.entry["document"], "http://localhost:5173/");
        let frame = log(&mut c, 2);
        assert_eq!(frame.origin, "https://ads.example");
        assert_eq!(frame.entry["document"], "https://ads.example/f");
        assert_eq!(log(&mut c, 99).origin, "", "unknown context: no origin");
        assert!(c.is_top_main_world(1));
        assert!(!c.is_top_main_world(2) && !c.is_top_main_world(3) && !c.is_top_main_world(99));
        // A request started on this document finishes after the next navigation: it keeps
        // the old navigation; the request that loaded the new document gets the new one.
        c.on_event(
            "Network.requestWillBeSent",
            &json!({"requestId": "old", "loaderId": "L1", "documentURL": "http://localhost:5173/",
                    "request": {"method": "GET", "url": "http://localhost:5173/slow"}}),
            0,
            &red,
        );
        c.on_event(
            "Network.requestWillBeSent",
            &json!({"requestId": "doc", "loaderId": "L2", "documentURL": "http://localhost:5173/two", "type": "Document",
                    "request": {"method": "GET", "url": "http://localhost:5173/two"}}),
            0,
            &red,
        );
        nav(&mut c, "http://localhost:5173/two", "L2");
        assert!(
            !c.is_top_main_world(1),
            "the old document's context is stale"
        );
        let done = |c: &mut Capture, id: &str| {
            c.on_event(
                "Network.loadingFinished",
                &json!({"requestId": id}),
                1,
                &red,
            )
            .unwrap()
        };
        let old = done(&mut c, "old");
        assert_eq!((old.origin.as_str(), old.nav), ("http://localhost:5173", 1));
        let doc = done(&mut c, "doc");
        assert_eq!(doc.nav, 2);
        assert_eq!(c.top_url, "http://localhost:5173/two");
        assert_eq!(done(&mut c, "never-started").origin, "");
        assert_eq!(
            origin_of("HTTP://User:pw@LocalHost:5173/a?b"),
            "http://localhost:5173"
        );
        assert_eq!(origin_of("about:blank"), "");
        assert_eq!(origin_of("data:text/html,x"), "");
    }
}
