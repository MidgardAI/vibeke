//! A fake CDP browser over a socket pair, for tests of code that drives [`crate::cdp::Cdp`]
//! (the server's agent-browser API, the MCP bridge) without launching Chromium.
//!
//! It answers the commands the agent browser uses with plausible results (contexts, targets,
//! flattened sessions, `Runtime.evaluate`, a 1×1 PNG for `Page.captureScreenshot`, a tiny
//! accessibility tree), records every command, and lets the test inject events.

use crate::cdp::{Cdp, Event, FrameDecoder};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

/// A 1×1 transparent PNG.
pub const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

/// A recorded command: (method, params, flattened session id).
pub type Call = (String, Value, Option<String>);

/// Custom answer: `Some(Ok(result))`, `Some(Err((code, message)))`, or `None` for the default.
pub type Handler =
    Arc<dyn Fn(&str, &Value, Option<&str>) -> Option<Result<Value, (i64, String)>> + Send + Sync>;

pub struct FakeBrowser {
    pub cdp: Arc<Cdp>,
    events: Option<Receiver<Event>>,
    pub calls: Arc<Mutex<Vec<Call>>>,
    writer: Arc<Mutex<UnixStream>>,
}

impl FakeBrowser {
    /// Start with the default answers only.
    pub fn start() -> FakeBrowser {
        Self::with_handler(Arc::new(|_, _, _| None))
    }

    pub fn with_handler(handler: Handler) -> FakeBrowser {
        let (ours, theirs) = UnixStream::pair().expect("socketpair");
        let writer = Arc::new(Mutex::new(theirs.try_clone().expect("clone")));
        let calls: Arc<Mutex<Vec<Call>>> = Arc::default();
        let (w2, c2) = (writer.clone(), calls.clone());
        std::thread::Builder::new()
            .name("fake-cdp".into())
            .spawn(move || serve(theirs, w2, c2, handler))
            .expect("spawn fake browser");
        let (cdp, events) = Cdp::new(ours.try_clone().expect("clone"), ours);
        FakeBrowser {
            cdp,
            events: Some(events),
            calls,
            writer,
        }
    }

    pub fn take_events(&mut self) -> Receiver<Event> {
        self.events.take().expect("events already taken")
    }

    /// Inject an event as if Chromium had sent it.
    pub fn emit(&self, method: &str, params: Value, session: Option<&str>) {
        emit_on(&self.writer, method, params, session);
    }

    /// A clonable event injector (for handlers that react to commands).
    pub fn emitter(&self) -> Emitter {
        Emitter(self.writer.clone())
    }

    pub fn methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.0.clone())
            .collect()
    }

    pub fn calls_of(&self, method: &str) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.0 == method)
            .cloned()
            .collect()
    }
}

#[derive(Clone)]
pub struct Emitter(Arc<Mutex<UnixStream>>);

impl Emitter {
    pub fn emit(&self, method: &str, params: Value, session: Option<&str>) {
        emit_on(&self.0, method, params, session);
    }
}

fn emit_on(w: &Mutex<UnixStream>, method: &str, params: Value, session: Option<&str>) {
    let mut ev = json!({"method": method, "params": params});
    if let Some(s) = session {
        ev["sessionId"] = json!(s);
    }
    let mut out = serde_json::to_vec(&ev).expect("json");
    out.push(0);
    let _ = w.lock().expect("writer").write_all(&out);
}

static NEXT: AtomicU64 = AtomicU64::new(1);

fn default_answer(method: &str, params: &Value, last_url: &mut String) -> Value {
    use base64::Engine as _;
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    match method {
        "Browser.getVersion" => {
            json!({"product": "HeadlessChrome/0.0-fake", "protocolVersion": "1.3"})
        }
        "Target.createBrowserContext" => json!({"browserContextId": format!("ctx-{n}")}),
        "Target.createTarget" => json!({"targetId": format!("target-{n}")}),
        "Target.attachToTarget" => json!({"sessionId": format!("session-{n}")}),
        "Page.navigate" => {
            *last_url = params["url"].as_str().unwrap_or("").to_string();
            json!({"frameId": "frame", "loaderId": format!("loader-{n}")})
        }
        "Page.captureScreenshot" => {
            json!({"data": base64::engine::general_purpose::STANDARD.encode(PNG_1X1)})
        }
        "Page.getLayoutMetrics" => json!({
            "cssContentSize": {"x": 0, "y": 0, "width": 1, "height": 1},
            "cssLayoutViewport": {"pageX": 0, "pageY": 0, "clientWidth": 1, "clientHeight": 1}
        }),
        "Accessibility.getFullAXTree" => json!({"nodes": [
            {"nodeId": "1", "ignored": false, "role": {"value": "RootWebArea"}, "name": {"value": "Fake page"}, "childIds": ["2"]},
            {"nodeId": "2", "parentId": "1", "ignored": false, "role": {"value": "button"}, "name": {"value": "Save"}, "childIds": []}
        ]}),
        "Runtime.evaluate" => {
            let expr = params["expression"].as_str().unwrap_or("");
            if expr == "document.readyState" {
                json!({"result": {"type": "string", "value": "complete"}})
            } else if expr.starts_with("location.href") {
                let url = if last_url.is_empty() {
                    "about:blank"
                } else {
                    last_url.as_str()
                };
                json!({"result": {"type": "string", "value": format!("{url}\nFake page")}})
            } else {
                json!({"result": {"type": "undefined"}})
            }
        }
        _ => json!({}),
    }
}

fn serve(
    mut r: UnixStream,
    w: Arc<Mutex<UnixStream>>,
    calls: Arc<Mutex<Vec<Call>>>,
    handler: Handler,
) {
    let mut d = FrameDecoder::new();
    let mut buf = [0u8; 65536];
    let mut last_url = String::new();
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let Ok(msgs) = d.push(&buf[..n]) else { return };
        for m in msgs {
            let Ok(v) = serde_json::from_slice::<Value>(&m) else {
                continue;
            };
            let id = v["id"].as_u64().unwrap_or(0);
            let method = v["method"].as_str().unwrap_or("").to_string();
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            let session = v["sessionId"].as_str().map(str::to_string);
            calls
                .lock()
                .unwrap()
                .push((method.clone(), params.clone(), session.clone()));
            let reply = match handler(&method, &params, session.as_deref()) {
                Some(Ok(r)) => json!({"id": id, "result": r}),
                Some(Err((code, message))) => {
                    json!({"id": id, "error": {"code": code, "message": message}})
                }
                None => {
                    json!({"id": id, "result": default_answer(&method, &params, &mut last_url)})
                }
            };
            let mut out = serde_json::to_vec(&reply).expect("json");
            out.push(0);
            if w.lock().unwrap().write_all(&out).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn answers_records_and_emits() {
        let mut f = FakeBrowser::with_handler(Arc::new(|m, _, _| {
            (m == "Fail.me").then(|| Err((-32000, "nope".to_string())))
        }));
        let ev = f.take_events();
        let r = f
            .cdp
            .call(None, "Target.createBrowserContext", json!({}))
            .unwrap();
        assert!(r["browserContextId"].as_str().unwrap().starts_with("ctx-"));
        assert!(f.cdp.call(None, "Fail.me", json!({})).is_err());
        f.emit(
            "Runtime.consoleAPICalled",
            json!({"type": "log"}),
            Some("s1"),
        );
        let e = ev.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(e.method, "Runtime.consoleAPICalled");
        assert_eq!(e.session_id.as_deref(), Some("s1"));
        assert_eq!(
            f.methods(),
            vec![
                "Target.createBrowserContext".to_string(),
                "Fail.me".to_string()
            ]
        );
    }
}
