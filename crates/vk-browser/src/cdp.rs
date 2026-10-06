//! Chrome DevTools Protocol over `--remote-debugging-pipe`.
//!
//! Chromium reads commands from fd 3 and writes responses/events to fd 4; every message is one
//! JSON object followed by a NUL byte. No websocket, no TCP port, nothing listening. Sessions
//! use flattened mode (`Target.attachToTarget {flatten: true}`), so every page command is a
//! top-level message carrying a `sessionId`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

/// Default timeout for a command round trip.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------------------------
// Framing

/// Serialise one CDP command as a NUL-terminated message.
pub fn encode_message(id: u64, method: &str, params: &Value, session: Option<&str>) -> Vec<u8> {
    let mut msg = json!({ "id": id, "method": method, "params": params });
    if let Some(s) = session {
        msg["sessionId"] = Value::String(s.to_owned());
    }
    let mut out = serde_json::to_vec(&msg).expect("json serialisation cannot fail");
    out.push(0);
    out
}

/// Splits a byte stream into NUL-delimited messages, keeping partial input across pushes.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    /// Hard cap on one message: screencast frames are base64 JPEG/PNG; a 4K PNG is ~30 MB.
    pub const MAX_MESSAGE: usize = 256 << 20;

    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns every complete message (without its NUL).
    pub fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        let mut rest = data;
        while let Some(pos) = rest.iter().position(|&b| b == 0) {
            if self.buf.is_empty() {
                out.push(rest[..pos].to_vec());
            } else {
                self.buf.extend_from_slice(&rest[..pos]);
                out.push(std::mem::take(&mut self.buf));
            }
            rest = &rest[pos + 1..];
        }
        self.buf.extend_from_slice(rest);
        if self.buf.len() > Self::MAX_MESSAGE {
            bail!("CDP message exceeds {} bytes", Self::MAX_MESSAGE);
        }
        Ok(out)
    }

    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

/// A parsed incoming message.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Response {
        id: u64,
        result: std::result::Result<Value, CdpError>,
    },
    Event {
        method: String,
        params: Value,
        session_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CDP error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for CdpError {}

/// Parse one message body (without the NUL).
pub fn parse_message(bytes: &[u8]) -> Result<Incoming> {
    let mut v: Value = serde_json::from_slice(bytes).context("CDP message is not JSON")?;
    if let Some(id) = v.get("id").and_then(Value::as_u64) {
        let result = if let Some(err) = v.get("error") {
            Err(CdpError {
                code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            })
        } else {
            Ok(v.get_mut("result").map(Value::take).unwrap_or(Value::Null))
        };
        return Ok(Incoming::Response { id, result });
    }
    let method = v
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("CDP message without id or method"))?
        .to_owned();
    Ok(Incoming::Event {
        method,
        params: v.get_mut("params").map(Value::take).unwrap_or(Value::Null),
        session_id: v
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

// ---------------------------------------------------------------------------------------------
// Connection

/// An event, stamped when the reader thread finished parsing it.
#[derive(Debug, Clone)]
pub struct Event {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
    pub received: Instant,
}

type Pending = Arc<Mutex<HashMap<u64, Sender<std::result::Result<Value, CdpError>>>>>;

/// A CDP connection over any byte transport (the pipe pair in production, a socket in tests).
pub struct Cdp {
    writer: Mutex<Box<dyn Write + Send>>,
    next_id: AtomicU64,
    pending: Pending,
}

impl Cdp {
    /// Start the reader thread. Events arrive on the returned receiver; responses are routed to
    /// the matching [`Cdp::call`]. The receiver closes when the transport does.
    pub fn new(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> (Arc<Cdp>, Receiver<Event>) {
        let pending: Pending = Arc::default();
        let (ev_tx, ev_rx) = mpsc::channel();
        let pending2 = pending.clone();
        std::thread::Builder::new()
            .name("cdp-reader".into())
            .spawn(move || reader_loop(reader, pending2, ev_tx))
            .expect("spawn cdp reader");
        let cdp = Arc::new(Cdp {
            writer: Mutex::new(Box::new(writer)),
            next_id: AtomicU64::new(1),
            pending,
        });
        (cdp, ev_rx)
    }

    fn write(&self, id: u64, method: &str, params: &Value, session: Option<&str>) -> Result<()> {
        let msg = encode_message(id, method, params, session);
        let mut w = self
            .writer
            .lock()
            .map_err(|_| anyhow!("CDP writer poisoned"))?;
        w.write_all(&msg).context("write to CDP pipe")?;
        w.flush().ok();
        Ok(())
    }

    /// Fire and forget: the response (and any error) is discarded. Returns the message id.
    pub fn send(&self, session: Option<&str>, method: &str, params: Value) -> Result<u64> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.write(id, method, &params, session)?;
        Ok(id)
    }

    /// Send a command and wait for its response.
    pub fn call(&self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        self.call_timeout(session, method, params, CALL_TIMEOUT)
    }

    pub fn call_timeout(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);
        if let Err(e) = self.write(id, method, &params, session) {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(e);
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!(e).context(method.to_owned())),
            Err(RecvTimeoutError::Timeout) => {
                self.pending.lock().expect("pending lock").remove(&id);
                bail!("{method}: no response within {timeout:?}")
            }
            Err(RecvTimeoutError::Disconnected) => bail!("{method}: CDP connection closed"),
        }
    }
}

fn reader_loop(mut reader: impl Read, pending: Pending, events: Sender<Event>) {
    let mut dec = FrameDecoder::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let msgs = match dec.push(&buf[..n]) {
            Ok(m) => m,
            Err(_) => break,
        };
        for m in msgs {
            match parse_message(&m) {
                Ok(Incoming::Response { id, result }) => {
                    if let Some(tx) = pending.lock().expect("pending lock").remove(&id) {
                        let _ = tx.send(result);
                    }
                }
                Ok(Incoming::Event {
                    method,
                    params,
                    session_id,
                }) => {
                    let _ = events.send(Event {
                        method,
                        params,
                        session_id,
                        received: Instant::now(),
                    });
                }
                Err(_) => {}
            }
        }
    }
    // Wake every waiter: dropping the senders turns their recv into Disconnected.
    pending.lock().expect("pending lock").clear();
}

// ---------------------------------------------------------------------------------------------
// Launching Chromium

/// Find a Chromium binary for the spike: `$VIBEKE_CHROMIUM`, then Playwright's cache
/// (newest revision first; `prefer_shell` picks `chrome-headless-shell` over full Chromium).
/// The user's own browser installs are deliberately not considered.
pub fn discover_chromium(prefer_shell: bool) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("VIBEKE_CHROMIUM") {
        return Some(PathBuf::from(p));
    }
    let home = std::env::var_os("HOME")?;
    let cache = PathBuf::from(home).join("Library/Caches/ms-playwright");
    let linux_cache = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".cache/ms-playwright"))
        .unwrap_or_default();
    let mut shells = Vec::new();
    let mut fulls = Vec::new();
    for root in [cache, linux_cache] {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let rev = |prefix: &str| {
                name.strip_prefix(prefix)
                    .and_then(|r| r.parse::<u32>().ok())
            };
            if let Some(r) = rev("chromium_headless_shell-") {
                for sub in [
                    "chrome-headless-shell-mac-arm64/chrome-headless-shell",
                    "chrome-headless-shell-mac-x64/chrome-headless-shell",
                    "chrome-headless-shell-linux64/chrome-headless-shell",
                    "chrome-linux/headless_shell",
                ] {
                    let p = e.path().join(sub);
                    if p.exists() {
                        shells.push((r, p));
                    }
                }
            } else if let Some(r) = rev("chromium-") {
                for sub in [
                    "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
                    "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
                    "chrome-mac-arm64/Chromium.app/Contents/MacOS/Chromium",
                    "chrome-linux64/chrome",
                    "chrome-linux/chrome",
                ] {
                    let p = e.path().join(sub);
                    if p.exists() {
                        fulls.push((r, p));
                    }
                }
            }
        }
    }
    shells.sort();
    fulls.sort();
    let (first, second) = if prefer_shell {
        (shells, fulls)
    } else {
        (fulls, shells)
    };
    first
        .into_iter()
        .next_back()
        .or_else(|| second.into_iter().next_back())
        .map(|(_, p)| p)
}

#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub binary: PathBuf,
    /// Fresh profile directory; never a user's real profile.
    pub user_data_dir: PathBuf,
    /// Pass `--headless=new` (full Chromium). The headless shell is always headless.
    pub headless_new: bool,
    pub extra_args: Vec<String>,
    /// `--force-device-scale-factor`: without it, screencast frames come out at CSS px even when
    /// `Emulation.setDeviceMetricsOverride` sets a DPR (screenshots honour the override,
    /// screencast does not). Set it to the host's DPR for crisp frames.
    pub device_scale_factor: Option<f64>,
    /// Where Chromium's stderr goes (None = discarded).
    pub stderr_log: Option<PathBuf>,
}

impl LaunchOptions {
    pub fn new(binary: impl Into<PathBuf>, user_data_dir: impl Into<PathBuf>) -> Self {
        LaunchOptions {
            binary: binary.into(),
            user_data_dir: user_data_dir.into(),
            headless_new: true,
            extra_args: Vec::new(),
            device_scale_factor: None,
            stderr_log: None,
        }
    }

    /// The full argument list.
    pub fn args(&self) -> Vec<String> {
        let mut a = vec![
            "--remote-debugging-pipe".to_owned(),
            format!("--user-data-dir={}", self.user_data_dir.display()),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-sync".into(),
            "--disable-background-networking".into(),
            "--disable-component-update".into(),
            "--disable-default-apps".into(),
            "--disable-extensions".into(),
            "--disable-features=Translate,MediaRouter,OptimizationHints".into(),
            "--disable-background-timer-throttling".into(),
            "--disable-renderer-backgrounding".into(),
            "--disable-backgrounding-occluded-windows".into(),
            "--metrics-recording-only".into(),
            "--password-store=basic".into(),
            "--use-mock-keychain".into(),
            "--mute-audio".into(),
        ];
        if self.headless_new {
            a.push("--headless=new".into());
        }
        if let Some(d) = self.device_scale_factor {
            a.push(format!("--force-device-scale-factor={d}"));
        }
        a.extend(self.extra_args.iter().cloned());
        a.push("about:blank".into());
        a
    }
}

/// A Chromium process driven over the pipe.
pub struct Browser {
    child: Child,
    pub cdp: Arc<Cdp>,
    events: Option<Receiver<Event>>,
}

fn pipe_high() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: fds is a valid 2-element array.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        bail!("pipe: {}", std::io::Error::last_os_error());
    }
    // Move both ends to fds >= 10 (close-on-exec) so the child's dup2 onto 3/4 never collides
    // with a source fd that is itself 3 or 4.
    let mut out = [0 as RawFd; 2];
    for i in 0..2 {
        // SAFETY: fds[i] is a valid open fd we own.
        let hi = unsafe { libc::fcntl(fds[i], libc::F_DUPFD_CLOEXEC, 10) };
        // SAFETY: closing the original we just duplicated.
        unsafe { libc::close(fds[i]) };
        if hi < 0 {
            bail!("fcntl: {}", std::io::Error::last_os_error());
        }
        out[i] = hi;
    }
    // SAFETY: both fds are freshly created and owned by us.
    Ok(unsafe { (OwnedFd::from_raw_fd(out[0]), OwnedFd::from_raw_fd(out[1])) })
}

impl Browser {
    pub fn launch(opts: &LaunchOptions) -> Result<Browser> {
        std::fs::create_dir_all(&opts.user_data_dir)?;
        // Chromium reads commands from fd 3 and writes to fd 4.
        let (cmd_r, cmd_w) = pipe_high()?;
        let (resp_r, resp_w) = pipe_high()?;
        let (child_in, child_out) = (cmd_r.as_raw_fd(), resp_w.as_raw_fd());
        let mut cmd = Command::new(&opts.binary);
        cmd.args(opts.args())
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        match &opts.stderr_log {
            Some(p) => cmd.stderr(File::create(p)?),
            None => cmd.stderr(Stdio::null()),
        };
        // SAFETY: only async-signal-safe calls (dup2) between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(child_in, 3) < 0 || libc::dup2(child_out, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("launch {}", opts.binary.display()))?;
        drop(cmd_r);
        drop(resp_w);
        let (cdp, events) = Cdp::new(File::from(resp_r), File::from(cmd_w));
        let b = Browser {
            child,
            cdp,
            events: Some(events),
        };
        b.cdp
            .call_timeout(
                None,
                "Browser.getVersion",
                json!({}),
                Duration::from_secs(20),
            )
            .context("Chromium did not answer on the debugging pipe")?;
        Ok(b)
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Take the event receiver (once).
    pub fn take_events(&mut self) -> Receiver<Event> {
        self.events.take().expect("events already taken")
    }

    pub fn version(&self) -> Result<Value> {
        self.cdp.call(None, "Browser.getVersion", json!({}))
    }

    /// Create a page target and attach to it (flattened session).
    pub fn new_page(&self, url: &str) -> Result<Page> {
        self.new_page_with(json!({ "url": url }))
    }

    /// `Target.createTarget` with explicit params (e.g. `enableBeginFrameControl`).
    pub fn new_page_with(&self, params: Value) -> Result<Page> {
        let r = self.cdp.call(None, "Target.createTarget", params)?;
        let target_id = r["targetId"]
            .as_str()
            .ok_or_else(|| anyhow!("createTarget: no targetId"))?
            .to_owned();
        let r = self.cdp.call(
            None,
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )?;
        let session = r["sessionId"]
            .as_str()
            .ok_or_else(|| anyhow!("attachToTarget: no sessionId"))?
            .to_owned();
        Ok(Page {
            cdp: self.cdp.clone(),
            session,
            target_id,
        })
    }

    pub fn close(mut self) -> Result<()> {
        self.shutdown();
        Ok(())
    }

    fn shutdown(&mut self) {
        let _ = self.cdp.send(None, "Browser.close", json!({}));
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            self.shutdown();
        }
    }
}

/// One attached page target.
#[derive(Clone)]
pub struct Page {
    cdp: Arc<Cdp>,
    pub session: String,
    pub target_id: String,
}

/// `Page.startScreencast` parameters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreencastParams {
    pub png: bool,
    pub quality: u8,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub every_nth_frame: u32,
}

impl Default for ScreencastParams {
    fn default() -> Self {
        ScreencastParams {
            png: false,
            quality: 80,
            max_width: None,
            max_height: None,
            every_nth_frame: 1,
        }
    }
}

/// One `Page.screencastFrame` event, still encoded.
#[derive(Debug, Clone)]
pub struct ScreencastFrame {
    pub session_id: u64,
    pub data: Vec<u8>,
    /// Chromium's frame timestamp (seconds), when present.
    pub timestamp: Option<f64>,
    pub received: Instant,
}

impl ScreencastFrame {
    /// Decode a `Page.screencastFrame` event's params.
    pub fn from_event(ev: &Event) -> Result<ScreencastFrame> {
        use base64::Engine as _;
        let p = &ev.params;
        let data = p["data"]
            .as_str()
            .ok_or_else(|| anyhow!("screencastFrame without data"))?;
        Ok(ScreencastFrame {
            session_id: p["sessionId"].as_u64().unwrap_or(0),
            data: base64::engine::general_purpose::STANDARD.decode(data)?,
            timestamp: p["metadata"]["timestamp"].as_f64(),
            received: ev.received,
        })
    }
}

impl Page {
    /// A page for an already attached flattened session.
    pub fn attached(cdp: Arc<Cdp>, session: String, target_id: String) -> Page {
        Page {
            cdp,
            session,
            target_id,
        }
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.cdp.call(Some(&self.session), method, params)
    }

    pub fn send(&self, method: &str, params: Value) -> Result<u64> {
        self.cdp.send(Some(&self.session), method, params)
    }

    pub fn enable(&self) -> Result<()> {
        self.call("Page.enable", json!({}))?;
        Ok(())
    }

    /// Size the viewport for a pane: `cols×cell_w` by `rows×cell_h` device pixels at `dpr`.
    /// CSS size is device size / dpr, so screenshots and screencast frames come out at exactly
    /// the pane's pixel size.
    pub fn set_viewport_for_cells(
        &self,
        cols: u32,
        rows: u32,
        cell_w: u32,
        cell_h: u32,
        dpr: f64,
    ) -> Result<(u32, u32)> {
        let (dev_w, dev_h) = (cols * cell_w, rows * cell_h);
        let css_w = (dev_w as f64 / dpr).round() as u32;
        let css_h = (dev_h as f64 / dpr).round() as u32;
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": css_w, "height": css_h, "deviceScaleFactor": dpr, "mobile": false }),
        )?;
        Ok((css_w, css_h))
    }

    /// Navigate and wait for the load event (events must be drained by the caller's receiver;
    /// this polls `document.readyState` instead so it doesn't consume them).
    pub fn navigate(&self, url: &str) -> Result<()> {
        self.call("Page.navigate", json!({ "url": url }))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let r = self.eval("document.readyState")?;
            if r.as_str() == Some("complete") {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        bail!("navigation to {url} did not complete")
    }

    pub fn eval(&self, expr: &str) -> Result<Value> {
        let r = self.call(
            "Runtime.evaluate",
            json!({ "expression": expr, "returnByValue": true, "awaitPromise": true }),
        )?;
        if let Some(ex) = r.get("exceptionDetails") {
            bail!("evaluate threw: {ex}");
        }
        Ok(r["result"]["value"].clone())
    }

    pub fn start_screencast(&self, p: ScreencastParams) -> Result<()> {
        let mut params = json!({
            "format": if p.png { "png" } else { "jpeg" },
            "everyNthFrame": p.every_nth_frame.max(1),
        });
        if !p.png {
            params["quality"] = json!(p.quality);
        }
        if let Some(w) = p.max_width {
            params["maxWidth"] = json!(w);
        }
        if let Some(h) = p.max_height {
            params["maxHeight"] = json!(h);
        }
        self.call("Page.startScreencast", params)?;
        Ok(())
    }

    pub fn stop_screencast(&self) -> Result<()> {
        self.call("Page.stopScreencast", json!({}))?;
        Ok(())
    }

    /// Ack a frame so Chromium sends the next one (fire and forget).
    pub fn ack_frame(&self, screencast_session_id: u64) -> Result<()> {
        self.send(
            "Page.screencastFrameAck",
            json!({ "sessionId": screencast_session_id }),
        )?;
        Ok(())
    }

    /// `Page.captureScreenshot`, decoded from base64.
    pub fn capture_screenshot(&self, png: bool, quality: u8) -> Result<Vec<u8>> {
        use base64::Engine as _;
        let mut params = json!({
            "format": if png { "png" } else { "jpeg" },
            "optimizeForSpeed": true,
        });
        if !png {
            params["quality"] = json!(quality);
        }
        let r = self.call("Page.captureScreenshot", params)?;
        let data = r["data"]
            .as_str()
            .ok_or_else(|| anyhow!("captureScreenshot: no data"))?;
        Ok(base64::engine::general_purpose::STANDARD.decode(data)?)
    }

    /// Mouse event in CSS px (`type`: mousePressed/mouseReleased/mouseMoved/mouseWheel).
    pub fn mouse(&self, params: Value) -> Result<u64> {
        self.send("Input.dispatchMouseEvent", params)
    }

    /// Wheel event at (x, y) CSS px with pixel deltas (smooth scrolling).
    pub fn wheel(&self, x: f64, y: f64, dx: f64, dy: f64) -> Result<u64> {
        self.mouse(json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": dx, "deltaY": dy }))
    }

    pub fn click(&self, x: f64, y: f64) -> Result<()> {
        for t in ["mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({ "type": t, "x": x, "y": y, "button": "left", "clickCount": 1 }),
            )?;
        }
        Ok(())
    }

    /// Dispatch the commands produced by [`crate::input::map_key`] (fire and forget; later
    /// `Runtime.evaluate` calls may overtake them).
    pub fn dispatch(&self, cmds: &[crate::input::CdpInput]) -> Result<()> {
        for c in cmds {
            let (m, p) = c.to_command();
            self.send(m, p)?;
        }
        Ok(())
    }

    /// Like [`Page::dispatch`] but waits for each command's response (Chromium answers input
    /// commands once the renderer handled the event).
    pub fn dispatch_wait(&self, cmds: &[crate::input::CdpInput]) -> Result<()> {
        for c in cmds {
            let (m, p) = c.to_command();
            self.call(m, p)?;
        }
        Ok(())
    }
}

/// Process-tree CPU time (user+system, seconds) for `root` and all descendants, via `ps`.
pub fn tree_cpu_seconds(root: u32) -> Result<f64> {
    let out = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,time="])
        .output()
        .context("run ps")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut rows = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(t)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        rows.push((pid, ppid, parse_ps_time(t).unwrap_or(0.0)));
    }
    Ok(sum_tree(&rows, root))
}

fn sum_tree(rows: &[(u32, u32, f64)], root: u32) -> f64 {
    let mut total = 0.0;
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(p) = stack.pop() {
        if !seen.insert(p) {
            continue;
        }
        for &(pid, ppid, t) in rows {
            if pid == p {
                total += t;
            }
            if ppid == p {
                stack.push(pid);
            }
        }
    }
    total
}

/// Parse `ps -o time` (`[[dd-]hh:]mm:ss.cc`).
pub fn parse_ps_time(s: &str) -> Option<f64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().ok()?, r),
        None => (0.0, s),
    };
    let mut secs = 0.0;
    for part in rest.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86400.0 + secs)
}

/// A temporary profile directory that is removed on drop.
pub struct TempProfile {
    pub path: PathBuf,
}

impl TempProfile {
    pub fn new(base: &Path, tag: &str) -> Result<TempProfile> {
        let path = base.join(format!(
            "vk-browser-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path)?;
        Ok(TempProfile { path })
    }
}

impl Drop for TempProfile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn encode_appends_nul_and_session() {
        let m = encode_message(7, "Page.navigate", &json!({"url": "x"}), Some("S1"));
        assert_eq!(*m.last().unwrap(), 0);
        let v: Value = serde_json::from_slice(&m[..m.len() - 1]).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["method"], "Page.navigate");
        assert_eq!(v["params"]["url"], "x");
        assert_eq!(v["sessionId"], "S1");
        let m = encode_message(1, "Browser.getVersion", &json!({}), None);
        let v: Value = serde_json::from_slice(&m[..m.len() - 1]).unwrap();
        assert!(v.get("sessionId").is_none());
    }

    #[test]
    fn decoder_handles_split_and_batched_messages() {
        let mut d = FrameDecoder::new();
        assert!(d.push(b"{\"id\":1").unwrap().is_empty());
        assert_eq!(d.pending(), 7);
        let out = d.push(b",\"result\":{}}\0{\"id\":2}\0{\"me").unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], b"{\"id\":1,\"result\":{}}");
        assert_eq!(out[1], b"{\"id\":2}");
        let out = d.push(b"thod\":\"X\"}\0").unwrap();
        assert_eq!(out, vec![b"{\"method\":\"X\"}".to_vec()]);
        assert_eq!(d.pending(), 0);
        // Byte-at-a-time.
        let msg = encode_message(3, "A.b", &json!({"k": [1, 2]}), None);
        let mut got = Vec::new();
        for b in &msg {
            got.extend(d.push(std::slice::from_ref(b)).unwrap());
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], msg[..msg.len() - 1]);
    }

    #[test]
    fn parse_response_error_and_event() {
        match parse_message(br#"{"id":4,"result":{"a":1}}"#).unwrap() {
            Incoming::Response { id, result } => {
                assert_eq!(id, 4);
                assert_eq!(result.unwrap()["a"], 1);
            }
            _ => panic!(),
        }
        match parse_message(br#"{"id":5,"error":{"code":-32601,"message":"nope"}}"#).unwrap() {
            Incoming::Response { result, .. } => {
                let e = result.unwrap_err();
                assert_eq!(e.code, -32601);
                assert_eq!(e.message, "nope");
            }
            _ => panic!(),
        }
        match parse_message(
            br#"{"method":"Page.screencastFrame","params":{"sessionId":9},"sessionId":"S"}"#,
        )
        .unwrap()
        {
            Incoming::Event {
                method,
                params,
                session_id,
            } => {
                assert_eq!(method, "Page.screencastFrame");
                assert_eq!(params["sessionId"], 9);
                assert_eq!(session_id.as_deref(), Some("S"));
            }
            _ => panic!(),
        }
        assert!(parse_message(b"{}").is_err());
        assert!(parse_message(b"nope").is_err());
    }

    /// A fake browser on the other end of a socket: answers every command and emits an event.
    #[test]
    fn connection_routes_responses_and_events() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let theirs_w = theirs.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut r = theirs;
            let mut w = theirs_w;
            let mut d = FrameDecoder::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = match r.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                for m in d.push(&buf[..n]).unwrap() {
                    let v: Value = serde_json::from_slice(&m).unwrap();
                    let id = v["id"].as_u64().unwrap();
                    let reply = if v["method"] == "Fail.me" {
                        json!({"id": id, "error": {"code": 1, "message": "boom"}})
                    } else {
                        json!({"id": id, "result": {"echo": v["method"], "session": v["sessionId"]}})
                    };
                    let ev =
                        json!({"method": "Test.event", "params": {"for": id}, "sessionId": "S"});
                    let mut out = serde_json::to_vec(&ev).unwrap();
                    out.push(0);
                    out.extend(serde_json::to_vec(&reply).unwrap());
                    out.push(0);
                    w.write_all(&out).unwrap();
                }
            }
        });
        let (cdp, events) = Cdp::new(ours.try_clone().unwrap(), ours);
        let r = cdp.call(Some("S"), "Page.enable", json!({})).unwrap();
        assert_eq!(r["echo"], "Page.enable");
        assert_eq!(r["session"], "S");
        let e = cdp.call(None, "Fail.me", json!({})).unwrap_err();
        assert!(format!("{e:#}").contains("boom"));
        let ev = events.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(ev.method, "Test.event");
        assert_eq!(ev.session_id.as_deref(), Some("S"));
        cdp.send(None, "Fire.forget", json!({})).unwrap();
        // Concurrent callers each get their own response.
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let c = cdp.clone();
                std::thread::spawn(move || {
                    let m = format!("M.m{i}");
                    let r = c.call(None, &m, json!({})).unwrap();
                    assert_eq!(r["echo"], m.as_str());
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
    }

    #[test]
    fn ps_time_and_tree_sum() {
        assert_eq!(parse_ps_time("0:01.50"), Some(1.5));
        assert_eq!(parse_ps_time("1:02:03.00"), Some(3723.0));
        assert_eq!(parse_ps_time("1-00:00:01.00"), Some(86401.0));
        assert_eq!(parse_ps_time("x"), None);
        let rows = [(1, 0, 1.0), (2, 1, 2.0), (3, 2, 4.0), (4, 9, 8.0)];
        assert_eq!(sum_tree(&rows, 1), 7.0);
        assert_eq!(sum_tree(&rows, 2), 6.0);
    }

    #[test]
    fn launch_args_are_isolated() {
        let o = LaunchOptions::new("/bin/chromium", "/tmp/p");
        let a = o.args();
        assert!(a.contains(&"--remote-debugging-pipe".to_owned()));
        assert!(a.contains(&"--user-data-dir=/tmp/p".to_owned()));
        assert!(a.contains(&"--use-mock-keychain".to_owned()));
        assert!(a.contains(&"--headless=new".to_owned()));
        assert!(!a.iter().any(|x| x.starts_with("--remote-debugging-port")));
        let mut o = o;
        o.device_scale_factor = Some(2.0);
        assert!(
            o.args()
                .contains(&"--force-device-scale-factor=2".to_owned())
        );
    }
}
