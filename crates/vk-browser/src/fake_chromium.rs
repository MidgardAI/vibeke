//! A fake Chromium speaking just enough CDP over the debugging pipe for the browser pane
//! (tests only): targets and flattened sessions, `Emulation.setDeviceMetricsOverride`,
//! screencast with real JPEG frames paced by acks, navigation and history, input commands
//! (logged), screenshots. Each page is a solid colour that changes on every input event and
//! navigation, so a test can see "a key reached the page" in the pixels as well as in the log.
//!
//! [`serve`] runs over any byte transport; [`spawn_pair`] returns a [`Cdp`] connected to a fake
//! on a socket pair; `vibeke debug fake-chromium` serves on fds 3/4 like the real thing.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Value, json};

use crate::cdp::{Cdp, Event, FrameDecoder};

#[derive(Debug, Clone)]
struct Page {
    target: String,
    url: String,
    history: Vec<String>,
    index: usize,
    css: (u32, u32),
    dpr: f64,
    screencast: bool,
    awaiting_ack: bool,
    dirty: bool,
    /// Bumped on input/navigation; the frame colour derives from it.
    generation: u32,
    frame_seq: u64,
    /// Bumped on every navigation/reload (the document's `performance.timeOrigin`).
    nav_seq: u32,
    /// `Page.startScreencast {maxWidth, maxHeight}`: frames are scaled down to fit.
    max: Option<(u32, u32)>,
    /// Last `Emulation.setDeviceMetricsOverride {deviceScaleFactor, mobile}`.
    emu_dpr: Option<f64>,
    mobile: bool,
    /// `Emulation.setUserAgentOverride`.
    ua: Option<String>,
    /// `Runtime.addBinding` names.
    bindings: Vec<String>,
    /// `Page.addScriptToEvaluateOnNewDocument` sources.
    scripts: Vec<String>,
}

impl Page {
    /// Frame size in device px: CSS × launch DPR, scaled down into `max` (aspect kept).
    fn frame_size(&self) -> (u32, u32) {
        let w = ((self.css.0 as f64 * self.dpr).round() as u32).max(1);
        let h = ((self.css.1 as f64 * self.dpr).round() as u32).max(1);
        match self.max {
            Some((mw, mh)) if mw > 0 && mh > 0 && (w > mw || h > mh) => {
                let k = (mw as f64 / w as f64).min(mh as f64 / h as f64);
                (
                    ((w as f64 * k).round() as u32).max(1),
                    ((h as f64 * k).round() as u32).max(1),
                )
            }
            _ => (w, h),
        }
    }
}

/// Shared state of one fake browser, inspectable by tests.
#[derive(Debug, Default)]
pub struct State {
    pages: HashMap<String, Page>,
    /// Every command received (method, params, session).
    pub log: Vec<(String, Value, Option<String>)>,
    pub frames_sent: u64,
    pub closed: bool,
    next: u32,
    /// The fake's end of the transport, to simulate a crash ([`State::crash`]).
    kill: Option<std::os::unix::net::UnixStream>,
    /// Frames are pixel noise (incompressible tiles: large media frames).
    pub noise: bool,
    /// Events queued by a test ([`State::inject`]), sent by the frame pump thread.
    inject: Vec<(String, Value, Option<String>)>,
}

impl State {
    /// Input commands (`Input.*`) received so far.
    pub fn inputs(&self) -> Vec<(String, Value)> {
        self.log
            .iter()
            .filter(|(m, _, _)| m.starts_with("Input."))
            .map(|(m, p, _)| (m.clone(), p.clone()))
            .collect()
    }
    pub fn count(&self, method: &str) -> usize {
        self.log.iter().filter(|(m, _, _)| m == method).count()
    }
    pub fn urls(&self) -> Vec<String> {
        self.pages.values().map(|p| p.url.clone()).collect()
    }
    pub fn screencasting(&self) -> usize {
        self.pages.values().filter(|p| p.screencast).count()
    }
    pub fn pages(&self) -> usize {
        self.pages.len()
    }
    /// CSS viewport of each page.
    pub fn viewports(&self) -> Vec<(u32, u32)> {
        self.pages.values().map(|p| p.css).collect()
    }
    /// Flattened session ids of the attached pages.
    pub fn sessions(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .pages
            .keys()
            .filter(|k| !k.starts_with("pending-"))
            .cloned()
            .collect();
        v.sort();
        v
    }
    /// Send an event as if Chromium had (on `session`, or browser-level with `None`).
    pub fn inject(&mut self, method: &str, params: Value, session: Option<&str>) {
        self.inject
            .push((method.to_string(), params, session.map(str::to_owned)));
    }
    /// `Runtime.addBinding` names of a page.
    pub fn bindings(&self, session: &str) -> Vec<String> {
        self.pages
            .get(session)
            .map(|p| p.bindings.clone())
            .unwrap_or_default()
    }
    /// `Page.addScriptToEvaluateOnNewDocument` sources of a page.
    pub fn scripts(&self, session: &str) -> Vec<String> {
        self.pages
            .get(session)
            .map(|p| p.scripts.clone())
            .unwrap_or_default()
    }
    /// Emulation of a page: (deviceScaleFactor override, mobile, user agent override).
    pub fn emulation(&self, session: &str) -> Option<(Option<f64>, bool, Option<String>)> {
        self.pages
            .get(session)
            .map(|p| (p.emu_dpr, p.mobile, p.ua.clone()))
    }
    /// Commands received for a method (params, session).
    pub fn calls(&self, method: &str) -> Vec<(Value, Option<String>)> {
        self.log
            .iter()
            .filter(|(m, _, _)| m == method)
            .map(|(_, p, s)| (p.clone(), s.clone()))
            .collect()
    }
    /// Simulate Chromium dying: the transport closes under the client (its event stream
    /// ends, like a crashed process's pipe).
    pub fn crash(&mut self) {
        if let Some(s) = self.kill.take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        self.closed = true;
    }
}

/// The fake's own user agent (`Browser.getVersion`); setting it again clears the override.
pub const DEFAULT_UA: &str = "Mozilla/5.0 (FakeChromium) HeadlessChrome/153.0.0.0";

/// The colour a page shows at `generation` (distinct for consecutive generations).
pub fn color_for(generation: u32) -> [u8; 3] {
    let g = generation as usize;
    const PALETTE: [[u8; 3]; 6] = [
        [240, 240, 240],
        [200, 30, 30],
        [30, 160, 60],
        [40, 70, 210],
        [230, 200, 20],
        [150, 40, 170],
    ];
    PALETTE[g % PALETTE.len()]
}

fn frame_jpeg(p: &Page, noise: bool) -> Vec<u8> {
    let (w, h) = p.frame_size();
    let c = color_for(p.generation);
    let mut img = image::RgbImage::from_pixel(w, h, image::Rgb(c));
    if noise {
        let mut x: u32 = 0x9e37_79b9 ^ p.generation;
        for px in img.pixels_mut() {
            // xorshift32
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *px = image::Rgb([x as u8, (x >> 8) as u8, (x >> 16) as u8]);
        }
    }
    // A dark band in the top-left whose width encodes the generation (visible in tiles).
    let band = ((p.generation % 8) + 1) * (w / 16).max(1);
    for y in 0..(h / 10).max(1) {
        for x in 0..band.min(w) {
            img.put_pixel(x, y, image::Rgb([10, 10, 10]));
        }
    }
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80);
    let _ = enc.encode(img.as_raw(), w, h, image::ExtendedColorType::Rgb8);
    out
}

fn png_of(p: &Page) -> Vec<u8> {
    let w = ((p.css.0 as f64 * p.dpr).round() as u32).max(1);
    let h = ((p.css.1 as f64 * p.dpr).round() as u32).max(1);
    let c = color_for(p.generation);
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..w * h {
        rgba.extend_from_slice(&[c[0], c[1], c[2], 255]);
    }
    crate::frame::encode_png(w, h, &rgba, true).unwrap_or_default()
}

struct Out<W: Write> {
    w: Mutex<W>,
}

impl<W: Write> Out<W> {
    fn send(&self, v: &Value) {
        let mut b = serde_json::to_vec(v).unwrap_or_default();
        b.push(0);
        let mut w = self.w.lock().unwrap();
        let _ = w.write_all(&b);
        let _ = w.flush();
    }
    fn event(&self, method: &str, params: Value, session: Option<&str>) {
        let mut v = json!({"method": method, "params": params});
        if let Some(s) = session {
            v["sessionId"] = json!(s);
        }
        self.send(&v);
    }
}

fn navigated<W: Write>(out: &Out<W>, session: &str, url: &str) {
    out.event(
        "Page.frameStartedLoading",
        json!({"frameId": "F"}),
        Some(session),
    );
    out.event(
        "Page.frameNavigated",
        json!({"frame": {"id": "F", "url": url, "loaderId": "L"}, "type": "Navigation"}),
        Some(session),
    );
    out.event(
        "Page.loadEventFired",
        json!({"timestamp": 1.0}),
        Some(session),
    );
    out.event(
        "Page.frameStoppedLoading",
        json!({"frameId": "F"}),
        Some(session),
    );
}

/// Serve CDP on `reader`/`writer` until the transport closes or `Browser.close`. `dpr` is
/// what `--force-device-scale-factor` would be (frames are CSS size × dpr).
pub fn serve(
    mut reader: impl Read + Send + 'static,
    writer: impl Write + Send + 'static,
    dpr: f64,
    state: Arc<Mutex<State>>,
) {
    let out = Arc::new(Out {
        w: Mutex::new(writer),
    });
    // Frame pump: one frame per dirty, screencasting page whose previous frame was acked.
    {
        let out = out.clone();
        let state = state.clone();
        std::thread::Builder::new()
            .name("fake-chromium-frames".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(15));
                    let mut todo = Vec::new();
                    let noise;
                    let injected;
                    {
                        let mut st = state.lock().unwrap();
                        if st.closed {
                            return;
                        }
                        injected = std::mem::take(&mut st.inject);
                        noise = st.noise;
                        let mut sent = 0;
                        for (sess, p) in st.pages.iter_mut() {
                            if p.screencast && p.dirty && !p.awaiting_ack {
                                p.dirty = false;
                                p.awaiting_ack = true;
                                p.frame_seq += 1;
                                todo.push((sess.clone(), p.clone()));
                                sent += 1;
                            }
                        }
                        st.frames_sent += sent;
                    }
                    for (m, p, sess) in injected {
                        out.event(&m, p, sess.as_deref());
                    }
                    for (sess, p) in todo {
                        let data =
                            base64::engine::general_purpose::STANDARD.encode(frame_jpeg(&p, noise));
                        out.event(
                            "Page.screencastFrame",
                            json!({"data": data, "sessionId": p.frame_seq,
                                   "metadata": {"timestamp": p.frame_seq as f64 / 100.0,
                                                "deviceWidth": p.css.0, "deviceHeight": p.css.1}}),
                            Some(&sess),
                        );
                    }
                }
            })
            .expect("spawn fake frame pump");
    }
    let mut dec = FrameDecoder::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let Ok(msgs) = dec.push(&buf[..n]) else {
            break;
        };
        for m in msgs {
            let Ok(v) = serde_json::from_slice::<Value>(&m) else {
                continue;
            };
            let id = v["id"].as_u64().unwrap_or(0);
            let method = v["method"].as_str().unwrap_or("").to_string();
            let params = v.get("params").cloned().unwrap_or(json!({}));
            let session = v["sessionId"].as_str().map(str::to_owned);
            let result = handle(&out, &state, &method, &params, session.as_deref(), dpr);
            out.send(&json!({"id": id, "result": result}));
            if method == "Browser.close" {
                state.lock().unwrap().closed = true;
                return;
            }
        }
    }
    state.lock().unwrap().closed = true;
}

fn handle<W: Write>(
    out: &Out<W>,
    state: &Arc<Mutex<State>>,
    method: &str,
    params: &Value,
    session: Option<&str>,
    dpr: f64,
) -> Value {
    let mut st = state.lock().unwrap();
    st.log.push((
        method.to_string(),
        params.clone(),
        session.map(str::to_owned),
    ));
    let sess = session.unwrap_or("").to_string();
    match method {
        "Browser.getVersion" => {
            json!({"product": "FakeChromium/1.0", "protocolVersion": "1.3", "userAgent": DEFAULT_UA})
        }
        "Target.createTarget" => {
            st.next += 1;
            let n = st.next;
            let url = params["url"].as_str().unwrap_or("about:blank").to_string();
            // The session is created on attach; park the page under its target id for now.
            st.pages.insert(
                format!("pending-T{n}"),
                Page {
                    target: format!("T{n}"),
                    url: url.clone(),
                    history: vec![url],
                    index: 0,
                    css: (800, 600),
                    dpr,
                    screencast: false,
                    awaiting_ack: false,
                    dirty: true,
                    generation: 0,
                    frame_seq: 0,
                    nav_seq: 0,
                    max: None,
                    emu_dpr: None,
                    mobile: false,
                    ua: None,
                    bindings: Vec::new(),
                    scripts: Vec::new(),
                },
            );
            json!({"targetId": format!("T{n}")})
        }
        "Target.attachToTarget" => {
            let t = params["targetId"].as_str().unwrap_or("").to_string();
            let key = format!("pending-{t}");
            match st.pages.remove(&key) {
                Some(p) => {
                    let s = format!("S{}", &t[1..]);
                    st.pages.insert(s.clone(), p);
                    json!({"sessionId": s})
                }
                None => json!({"sessionId": "S-missing"}),
            }
        }
        "Target.closeTarget" => {
            let t = params["targetId"].as_str().unwrap_or("");
            let s = st
                .pages
                .iter()
                .find(|(_, p)| p.target == t)
                .map(|(s, _)| s.clone());
            if let Some(s) = s {
                st.pages.remove(&s);
                drop(st);
                out.event("Target.targetDestroyed", json!({"targetId": t}), None);
            }
            json!({"success": true})
        }
        "Emulation.setDeviceMetricsOverride" => {
            if let Some(p) = st.pages.get_mut(&sess) {
                p.css = (
                    params["width"].as_u64().unwrap_or(800) as u32,
                    params["height"].as_u64().unwrap_or(600) as u32,
                );
                p.emu_dpr = params["deviceScaleFactor"].as_f64();
                p.mobile = params["mobile"].as_bool().unwrap_or(false);
                p.dirty = true;
            }
            json!({})
        }
        "Emulation.setUserAgentOverride" => {
            if let Some(p) = st.pages.get_mut(&sess) {
                let ua = params["userAgent"].as_str().unwrap_or("");
                p.ua = (!ua.is_empty() && ua != DEFAULT_UA).then(|| ua.to_string());
            }
            json!({})
        }
        "Runtime.addBinding" => {
            if let Some(p) = st.pages.get_mut(&sess)
                && let Some(n) = params["name"].as_str()
            {
                p.bindings.push(n.to_string());
            }
            json!({})
        }
        "Page.addScriptToEvaluateOnNewDocument" => {
            if let Some(p) = st.pages.get_mut(&sess)
                && let Some(src) = params["source"].as_str()
            {
                p.scripts.push(src.to_string());
            }
            json!({"identifier": "1"})
        }
        "Page.startScreencast" => {
            if let Some(p) = st.pages.get_mut(&sess) {
                p.max = match (params["maxWidth"].as_u64(), params["maxHeight"].as_u64()) {
                    (Some(w), Some(h)) => Some((w as u32, h as u32)),
                    _ => None,
                };
                p.screencast = true;
                p.awaiting_ack = false;
                p.dirty = true;
            }
            json!({})
        }
        "Page.stopScreencast" => {
            if let Some(p) = st.pages.get_mut(&sess) {
                p.screencast = false;
            }
            json!({})
        }
        "Page.screencastFrameAck" => {
            if let Some(p) = st.pages.get_mut(&sess) {
                p.awaiting_ack = false;
            }
            json!({})
        }
        "Page.navigate" => {
            let url = params["url"].as_str().unwrap_or("about:blank").to_string();
            if let Some(p) = st.pages.get_mut(&sess) {
                p.history.truncate(p.index + 1);
                p.history.push(url.clone());
                p.index = p.history.len() - 1;
                p.url = url.clone();
                p.generation += 1;
                p.nav_seq += 1;
                p.dirty = true;
            }
            drop(st);
            navigated(out, &sess, &url);
            // The document's request and a console line, as a page would produce them.
            out.event(
                "Network.requestWillBeSent",
                json!({"requestId": format!("R{url}"), "type": "Document",
                       "request": {"method": "GET", "url": url}}),
                Some(&sess),
            );
            out.event(
                "Network.responseReceived",
                json!({"requestId": format!("R{url}"), "response": {"status": 200, "mimeType": "text/html"}}),
                Some(&sess),
            );
            out.event(
                "Network.loadingFinished",
                json!({"requestId": format!("R{url}")}),
                Some(&sess),
            );
            out.event(
                "Runtime.consoleAPICalled",
                json!({"type": "log", "args": [{"type": "string", "value": format!("loaded {url}")}]}),
                Some(&sess),
            );
            json!({"frameId": "F", "loaderId": "L"})
        }
        "Page.getNavigationHistory" => match st.pages.get(&sess) {
            Some(p) => json!({
                "currentIndex": p.index,
                "entries": p.history.iter().enumerate()
                    .map(|(i, u)| json!({"id": i, "url": u, "userTypedURL": u, "title": format!("page {i}"), "transitionType": "typed"}))
                    .collect::<Vec<_>>(),
            }),
            None => json!({"currentIndex": 0, "entries": []}),
        },
        "Page.navigateToHistoryEntry" => {
            let i = params["entryId"].as_u64().unwrap_or(0) as usize;
            let mut url = None;
            if let Some(p) = st.pages.get_mut(&sess)
                && i < p.history.len()
            {
                p.index = i;
                p.url = p.history[i].clone();
                p.generation += 1;
                p.nav_seq += 1;
                p.dirty = true;
                url = Some(p.url.clone());
            }
            drop(st);
            if let Some(u) = url {
                navigated(out, &sess, &u);
            }
            json!({})
        }
        "Page.reload" => {
            let mut url = None;
            if let Some(p) = st.pages.get_mut(&sess) {
                p.generation += 1;
                p.nav_seq += 1;
                p.dirty = true;
                url = Some(p.url.clone());
            }
            drop(st);
            if let Some(u) = url {
                navigated(out, &sess, &u);
            }
            json!({})
        }
        "Page.captureScreenshot" => match st.pages.get(&sess) {
            Some(p) => {
                json!({"data": base64::engine::general_purpose::STANDARD.encode(png_of(p))})
            }
            None => json!({"data": ""}),
        },
        "Input.dispatchKeyEvent" | "Input.insertText" | "Input.dispatchMouseEvent" => {
            if method == "Input.insertText" {
                let text = params["text"].as_str().unwrap_or("").to_string();
                page_script(&mut st, &sess, &text);
            }
            let bump = match method {
                "Input.dispatchKeyEvent" => params["type"] != "keyUp",
                "Input.insertText" => true,
                _ => matches!(
                    params["type"].as_str(),
                    Some("mousePressed") | Some("mouseWheel")
                ),
            };
            if bump && let Some(p) = st.pages.get_mut(&sess) {
                p.generation += 1;
                p.dirty = true;
            }
            json!({})
        }
        "Runtime.evaluate" => {
            let expr = params["expression"].as_str().unwrap_or("");
            let v = match (expr, st.pages.get(&sess)) {
                ("location.href", Some(p)) => json!(p.url),
                ("document.readyState", _) => json!("complete"),
                // The screenshot's document-identity read (vk-server screenshots).
                (e, Some(p)) if e.contains("__VIBEKE_BUILD__") => {
                    let origin = url::Url::parse(&p.url)
                        .map(|u| u.origin().ascii_serialization())
                        .unwrap_or_default();
                    json!({"href": p.url, "origin": origin, "title": format!("page {}", p.index),
                           "time_origin_ms": 1_700_000_000_000.0 + p.nav_seq as f64, "build": null})
                }
                _ => Value::Null,
            };
            json!({"result": {"type": "string", "value": v}})
        }
        _ => json!({}),
    }
}

/// What the fake page does with typed text (so end-to-end tests can drive page behaviour
/// through the pipe): `log:<t>` / `warn:<t>` log to the console, `error:<t>` throws,
/// `fail:<url>` makes a failed request, `copy:<t>` writes `<t>` to the clipboard through the
/// page's binding (as the injected clipboard hook would), `chooser` opens a file chooser.
fn page_script(st: &mut State, sess: &str, text: &str) {
    let s = Some(sess);
    if let Some(t) = text
        .strip_prefix("log:")
        .or_else(|| text.strip_prefix("warn:"))
    {
        let ty = if text.starts_with("warn:") {
            "warning"
        } else {
            "log"
        };
        st.inject(
            "Runtime.consoleAPICalled",
            json!({"type": ty, "args": [{"type": "string", "value": t}]}),
            s,
        );
    } else if let Some(t) = text.strip_prefix("error:") {
        st.inject(
            "Runtime.exceptionThrown",
            json!({"exceptionDetails": {"text": "Uncaught", "exception": {"description": format!("Error: {t}")}}}),
            s,
        );
    } else if let Some(u) = text.strip_prefix("fail:") {
        st.inject(
            "Network.requestWillBeSent",
            json!({"requestId": format!("F{u}"), "type": "Fetch", "request": {"method": "GET", "url": u}}),
            s,
        );
        st.inject(
            "Network.loadingFailed",
            json!({"requestId": format!("F{u}"), "errorText": "net::ERR_CONNECTION_REFUSED"}),
            s,
        );
    } else if let Some(t) = text.strip_prefix("copy:") {
        if let Some(b) = st.pages.get(sess).and_then(|p| p.bindings.last().cloned()) {
            st.inject(
                "Runtime.bindingCalled",
                json!({"name": b, "payload": t, "executionContextId": 1}),
                s,
            );
        }
    } else if text == "chooser" {
        st.inject(
            "Page.fileChooserOpened",
            json!({"frameId": "F", "mode": "selectSingle", "backendNodeId": 42}),
            s,
        );
    }
}

/// A fake browser on a socket pair: the client side as a [`Cdp`] plus its events, and the
/// shared state.
pub fn spawn_pair(dpr: f64) -> (Arc<Cdp>, Receiver<Event>, Arc<Mutex<State>>) {
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let state = Arc::new(Mutex::new(State::default()));
    state.lock().unwrap().kill = theirs.try_clone().ok();
    let st = state.clone();
    let w = theirs.try_clone().expect("clone socket");
    std::thread::Builder::new()
        .name("fake-chromium".into())
        .spawn(move || serve(theirs, w, dpr, st))
        .expect("spawn fake chromium");
    let (cdp, events) = Cdp::new(ours.try_clone().expect("clone socket"), ours);
    (cdp, events, state)
}

/// `--force-device-scale-factor=<x>` from an argument list (default 1).
pub fn dpr_from_args(args: &[String]) -> f64 {
    args.iter()
        .find_map(|a| a.strip_prefix("--force-device-scale-factor="))
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_screencast_and_input() {
        let (cdp, events, state) = spawn_pair(2.0);
        let r = cdp
            .call(None, "Target.createTarget", json!({"url": "http://x/"}))
            .unwrap();
        let t = r["targetId"].as_str().unwrap().to_string();
        let s = cdp
            .call(
                None,
                "Target.attachToTarget",
                json!({"targetId": t, "flatten": true}),
            )
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        cdp.call(
            Some(&s),
            "Emulation.setDeviceMetricsOverride",
            json!({"width": 64, "height": 32, "deviceScaleFactor": 2}),
        )
        .unwrap();
        cdp.call(Some(&s), "Page.startScreencast", json!({"format": "jpeg"}))
            .unwrap();
        let ev = loop {
            let e = events.recv_timeout(Duration::from_secs(5)).unwrap();
            if e.method == "Page.screencastFrame" {
                break e;
            }
        };
        let f = crate::cdp::ScreencastFrame::from_event(&ev).unwrap();
        let img = crate::frame::decode(&f.data).unwrap();
        assert_eq!((img.width, img.height), (128, 64));
        let px = img.pixel(120, 60);
        assert!(px[0] > 200 && px[1] > 200, "first page colour {px:?}");
        // No second frame until acked, even when dirty.
        cdp.call(Some(&s), "Input.insertText", json!({"text": "a"}))
            .unwrap();
        assert!(
            events
                .recv_timeout(Duration::from_millis(200))
                .map(|e| e.method != "Page.screencastFrame")
                .unwrap_or(true)
        );
        cdp.call(
            Some(&s),
            "Page.screencastFrameAck",
            json!({"sessionId": f.session_id}),
        )
        .unwrap();
        let ev = loop {
            let e = events.recv_timeout(Duration::from_secs(5)).unwrap();
            if e.method == "Page.screencastFrame" {
                break e;
            }
        };
        let img = crate::frame::decode(&crate::cdp::ScreencastFrame::from_event(&ev).unwrap().data)
            .unwrap();
        let px = img.pixel(120, 60);
        assert!(px[0] > 150 && px[1] < 80, "second page colour {px:?}");
        assert_eq!(state.lock().unwrap().inputs().len(), 1);
    }

    #[test]
    fn fake_bounds_frames_records_emulation_and_runs_page_scripts() {
        let (cdp, events, state) = spawn_pair(2.0);
        let t = cdp
            .call(None, "Target.createTarget", json!({"url": "about:blank"}))
            .unwrap()["targetId"]
            .as_str()
            .unwrap()
            .to_string();
        let s = cdp
            .call(
                None,
                "Target.attachToTarget",
                json!({"targetId": t, "flatten": true}),
            )
            .unwrap()["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
        cdp.call(
            Some(&s),
            "Emulation.setDeviceMetricsOverride",
            json!({"width": 393, "height": 852, "deviceScaleFactor": 3, "mobile": true}),
        )
        .unwrap();
        cdp.call(
            Some(&s),
            "Emulation.setUserAgentOverride",
            json!({"userAgent": "UA-phone"}),
        )
        .unwrap();
        cdp.call(Some(&s), "Runtime.addBinding", json!({"name": "__b"}))
            .unwrap();
        cdp.call(
            Some(&s),
            "Page.startScreencast",
            json!({"format": "jpeg", "maxWidth": 200, "maxHeight": 300}),
        )
        .unwrap();
        let frame = loop {
            let e = events.recv_timeout(Duration::from_secs(5)).unwrap();
            if e.method == "Page.screencastFrame" {
                break e;
            }
        };
        let img = crate::frame::decode(
            &crate::cdp::ScreencastFrame::from_event(&frame)
                .unwrap()
                .data,
        )
        .unwrap();
        // 393×852 CSS at the launch DPR 2 = 786×1704, scaled into 200×300.
        assert_eq!(img.height, 300);
        assert!((img.width as i32 - 138).abs() <= 1, "{}", img.width);
        assert_eq!(
            state.lock().unwrap().emulation(&s),
            Some((Some(3.0), true, Some("UA-phone".into())))
        );
        // Setting the browser's own UA clears the override.
        cdp.call(
            Some(&s),
            "Emulation.setUserAgentOverride",
            json!({"userAgent": DEFAULT_UA}),
        )
        .unwrap();
        assert_eq!(state.lock().unwrap().emulation(&s).unwrap().2, None);
        // Typed `copy:` text calls the page's binding.
        cdp.call(Some(&s), "Input.insertText", json!({"text": "copy:hi"}))
            .unwrap();
        let ev = loop {
            let e = events.recv_timeout(Duration::from_secs(5)).unwrap();
            if e.method == "Runtime.bindingCalled" {
                break e;
            }
        };
        assert_eq!(ev.params["name"], "__b");
        assert_eq!(ev.params["payload"], "hi");
        assert_eq!(ev.session_id.as_deref(), Some(s.as_str()));
    }
}
