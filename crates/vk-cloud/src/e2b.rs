//! E2B (spec 17 §3.3). An E2B sandbox is a Firecracker microVM that runs until its timeout, then
//! pauses (with `autoPause`) or dies. Pause keeps memory, so running processes continue after
//! resume. The control plane manages sandboxes; commands and files go to `envd`, the agent
//! inside each sandbox.
//!
//! - Control plane (`https://api.e2b.app`, header `X-API-Key`): `POST /v2/sandboxes`,
//!   `GET /v2/sandboxes` (both states, `X-Next-Token` pages), `GET`/`DELETE /sandboxes/{id}`,
//!   `POST /sandboxes/{id}/pause`, `POST /v2/sandboxes/{id}/connect` (resumes and returns the
//!   envd access token), `POST /sandboxes/{id}/snapshots`.
//! - envd (`https://49983-{id}.{domain}`, header `X-Access-Token`): the `process.Process`
//!   Connect RPC service with JSON. `Start` and `Connect` are server streams: every message is
//!   an envelope of one flag byte and a big-endian `u32` length ([`envelope`], [`Frames`]); the
//!   last one has flag `0x02` and carries the end-of-stream JSON with an optional error.
//!   `SendInput`, `Update`, `SendSignal`, `CloseStdin` and `List` are unary JSON calls. Bytes
//!   are base64 in JSON. Files go to `POST /files?path=&username=` as multipart.
//! - The session id is the process id. Each process gets a tag (`vk-tty-…`/`vk-pipe-…`) so
//!   [`Provider::sessions`] and [`Provider::attach`] know whether it has a terminal.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use url::Url;

use crate::sprites::parse_rfc3339;
use crate::{
    Account, AuthMethod, BoxFut, BoxState, Caps, CloudError, CreateSpec, ErrorKind, ExecReq, In,
    Out, Provider, ProviderConfig, RemoteBox, Result, Secret, Session, SessionInfo, naming,
};

pub const DEFAULT_API: &str = "https://api.e2b.app";
pub const DEFAULT_DOMAIN: &str = "e2b.app";
pub const DEFAULT_TEMPLATE: &str = "base";
/// Sandbox lifetime when neither the request nor the config sets one.
pub const DEFAULT_TIMEOUT_S: u64 = 3600;
/// The longest sandbox lifetime E2B allows (Pro plan).
pub const MAX_RUNTIME_S: u64 = 86_400;
/// Port of envd inside every sandbox.
pub const ENVD_PORT: u16 = 49983;
/// The user commands and files run as (the default user of E2B templates).
pub const USER: &str = "user";

/// Envelope flags of the Connect protocol.
pub const FLAG_COMPRESSED: u8 = 0x01;
pub const FLAG_END_STREAM: u8 = 0x02;

const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
const UNARY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `Start` or `Connect` may take to report the process id.
const START_WAIT: Duration = Duration::from_secs(30);
/// envd sends a keepalive event this often (seconds) when asked by `Keepalive-Ping-Interval`.
const KEEPALIVE_S: u64 = 50;
/// A stream with no bytes for this long is treated as lost.
const IDLE_LIMIT: Duration = Duration::from_secs(KEEPALIVE_S * 3 + 30);
/// Largest envelope accepted from envd.
const MAX_FRAME: usize = 16 * 1024 * 1024;
/// Input bytes merged into one `SendInput` call when they queue up.
const COALESCE: usize = 1024 * 1024;

/// How to reach envd in one sandbox. Not `Debug`: it holds the access token.
#[derive(Clone)]
struct Access {
    token: Option<String>,
    domain: String,
}

pub struct E2b {
    pub cfg: ProviderConfig,
    http: reqwest::Client,
    /// envd access per sandbox id, from create, get and connect. A new process fills it with a
    /// connect call (which also resumes a paused sandbox).
    access: Mutex<HashMap<String, Access>>,
    /// Tests: envd base URL instead of `https://49983-{id}.{domain}`.
    envd_url: Option<String>,
}

impl E2b {
    pub fn new(cfg: ProviderConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("vibeke/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        E2b {
            cfg,
            http,
            access: Mutex::new(HashMap::new()),
            envd_url: None,
        }
    }

    fn base(&self) -> &str {
        self.cfg
            .api_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(DEFAULT_API)
    }

    /// Sandbox domain when the API does not name one: `api.<domain>` of the API URL, else
    /// `e2b.app`.
    fn default_domain(&self) -> String {
        Url::parse(self.base())
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .and_then(|h| h.strip_prefix("api.").map(str::to_string))
            .filter(|d| d.contains('.'))
            .unwrap_or_else(|| DEFAULT_DOMAIN.to_string())
    }

    /// Sandbox lifetime in seconds: the request, else the config, else one hour; at most a day.
    fn timeout_s(&self, requested: u64) -> u64 {
        let t = if requested > 0 {
            requested
        } else {
            self.cfg.timeout_s()
        };
        let t = if t == 0 { DEFAULT_TIMEOUT_S } else { t };
        t.min(MAX_RUNTIME_S)
    }

    /// `<base>/<segs…>?<query>`; segments are percent-encoded.
    fn url(&self, segs: &[&str], query: &[(&str, String)]) -> Result<Url> {
        let mut u = Url::parse(self.base().trim_end_matches('/'))
            .map_err(|e| CloudError::new(ErrorKind::InvalidParams, format!("bad api_url: {e}")))?;
        {
            let mut p = u.path_segments_mut().map_err(|_| {
                CloudError::new(ErrorKind::InvalidParams, "api_url can't take a path")
            })?;
            p.pop_if_empty();
            for s in segs {
                if s.is_empty() {
                    return Err(CloudError::new(ErrorKind::InvalidParams, "empty id"));
                }
                p.push(s);
            }
        }
        if !query.is_empty() {
            let mut q = u.query_pairs_mut();
            for (k, v) in query {
                q.append_pair(k, v);
            }
        }
        Ok(u)
    }

    fn req(
        &self,
        cred: &Secret,
        method: reqwest::Method,
        url: Url,
    ) -> Result<reqwest::RequestBuilder> {
        check_key(cred)?;
        Ok(self
            .http
            .request(method, url)
            .header("X-API-Key", cred.expose())
            .timeout(HTTP_TIMEOUT))
    }

    fn post_json(&self, cred: &Secret, url: Url, body: &Value) -> Result<reqwest::RequestBuilder> {
        Ok(self
            .req(cred, reqwest::Method::POST, url)?
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string()))
    }

    /// Keep the envd token and domain from a `Sandbox` or `SandboxDetail` response.
    fn remember(&self, id: &str, v: &Value) {
        let token = v
            .get("envdAccessToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        let domain = v
            .get("domain")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.default_domain());
        if let Ok(mut m) = self.access.lock() {
            m.insert(id.to_string(), Access { token, domain });
        }
    }

    fn forget(&self, id: &str) {
        if let Ok(mut m) = self.access.lock() {
            m.remove(id);
        }
    }

    fn cached(&self, id: &str) -> Option<Access> {
        self.access.lock().ok().and_then(|m| m.get(id).cloned())
    }

    async fn get_box(&self, cred: &Secret, id: &str) -> Result<RemoteBox> {
        let (_, body) = send(self.req(
            cred,
            reqwest::Method::GET,
            self.url(&["sandboxes", id], &[])?,
        )?)
        .await?;
        let v: Value = serde_json::from_slice(&body)
            .map_err(|e| CloudError::unavailable(format!("unexpected E2B response: {e}")))?;
        self.remember(id, &v);
        parse_box(&v).ok_or_else(|| CloudError::unavailable("unexpected E2B response"))
    }

    /// `POST /v2/sandboxes/{id}/connect`: resumes a paused sandbox, extends its lifetime and
    /// returns the envd access token.
    async fn connect_box(&self, cred: &Secret, id: &str) -> Result<Access> {
        let url = self.url(&["v2", "sandboxes", id, "connect"], &[])?;
        let body = json!({"timeout": self.timeout_s(0)});
        let (_, resp) = send(self.post_json(cred, url, &body)?).await?;
        let v: Value = serde_json::from_slice(&resp).unwrap_or(Value::Null);
        self.remember(id, &v);
        Ok(self.cached(id).unwrap_or(Access {
            token: None,
            domain: self.default_domain(),
        }))
    }

    /// envd access for `id`: the cached one, or a fresh one from connect when `refresh` is set
    /// or nothing is cached.
    async fn access_for(&self, cred: &Secret, id: &str, refresh: bool) -> Result<Access> {
        if !refresh && let Some(a) = self.cached(id) {
            return Ok(a);
        }
        self.connect_box(cred, id).await
    }

    fn envd(&self, id: &str, a: &Access) -> Envd {
        let base = match &self.envd_url {
            Some(u) => u.trim_end_matches('/').to_string(),
            None => format!("https://{ENVD_PORT}-{id}.{}", a.domain),
        };
        Envd {
            http: self.http.clone(),
            base,
            token: a.token.clone(),
            sandbox: id.to_string(),
        }
    }

    /// Run `op` against envd; when it fails in a way a stale token or a paused sandbox
    /// explains, refresh the access with connect and try once more.
    async fn with_envd<T, F, Fut>(&self, cred: &Secret, id: &str, op: F) -> Result<(Envd, T)>
    where
        F: Fn(Envd) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut refreshed = false;
        loop {
            let fresh = refreshed || self.cached(id).is_none();
            let a = self.access_for(cred, id, refreshed).await?;
            let e = self.envd(id, &a);
            match op(e.clone()).await {
                Ok(t) => return Ok((e, t)),
                Err(err)
                    if !fresh
                        && matches!(
                            err.kind,
                            ErrorKind::NeedsAuth | ErrorKind::Unavailable | ErrorKind::NotFound
                        ) =>
                {
                    refreshed = true;
                }
                // The API key works (connect succeeded) but envd refused: not a sign-in problem.
                Err(err) if err.kind == ErrorKind::NeedsAuth => {
                    return Err(CloudError::unavailable(format!(
                        "the E2B sandbox refused access: {}",
                        err.message
                    )));
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Open a `Start` or `Connect` stream and start the bridge task.
    async fn open(
        &self,
        cred: &Secret,
        id: &str,
        method: &'static str,
        body: Value,
        tty: bool,
        detachable: bool,
    ) -> Result<(Envd, Session)> {
        let body = &body;
        let (envd, (resp, frames, pid, pending)) = self
            .with_envd(
                cred,
                id,
                |e| async move { handshake(&e, method, body).await },
            )
            .await?;
        let (in_tx, in_rx) = mpsc::channel(64);
        let (out_tx, out_rx) = mpsc::channel(256);
        tokio::spawn(pump(
            Pump {
                ctl: Ctl {
                    envd: envd.clone(),
                    pid,
                    tty,
                    detachable,
                },
                resp,
                frames,
                pending,
            },
            in_rx,
            out_tx,
        ));
        Ok((
            envd,
            Session {
                id: pid.to_string(),
                tty,
                input: in_tx,
                output: out_rx,
            },
        ))
    }

    async fn processes(&self, cred: &Secret, id: &str) -> Result<Vec<Proc>> {
        let (_, v) = self
            .with_envd(
                cred,
                id,
                |e| async move { e.unary("List", &json!({})).await },
            )
            .await?;
        Ok(procs_from(&v))
    }

    /// Run a short pipe command; its exit code and stderr.
    async fn run_quiet(&self, cred: &Secret, id: &str, argv: Vec<String>) -> Result<(i32, String)> {
        let mut s = self
            .exec(
                cred,
                id,
                ExecReq {
                    argv,
                    ..Default::default()
                },
            )
            .await?;
        let _ = s.input.send(In::Eof).await;
        let mut err = Vec::new();
        let r = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match s.output.recv().await {
                    Some(Out::Exit(c)) => return Ok(c),
                    Some(Out::Stderr(d)) => err.extend_from_slice(&d),
                    Some(Out::Lost(m)) => return Err(CloudError::unavailable(m)),
                    Some(_) => {}
                    None => return Err(CloudError::unavailable("the command's stream ended")),
                }
            }
        })
        .await
        .map_err(|_| CloudError::unavailable("the command did not finish in time"))??;
        Ok((r, String::from_utf8_lossy(&err).trim().to_string()))
    }
}

/// An E2B API key is a header value; reject one that can't be sent as one.
fn check_key(cred: &Secret) -> Result<()> {
    if cred.is_empty() {
        return Err(CloudError::needs_auth("e2b"));
    }
    if !cred.expose().bytes().all(|b| (0x21..0x7f).contains(&b)) {
        return Err(CloudError::new(
            ErrorKind::NeedsAuth,
            "the E2B API key contains characters a header can't carry",
        ));
    }
    Ok(())
}

/// Send a request; a non-2xx status becomes a [`CloudError`].
async fn send(rb: reqwest::RequestBuilder) -> Result<(reqwest::header::HeaderMap, Vec<u8>)> {
    let resp = rb.send().await.map_err(transport)?;
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp.bytes().await.map_err(transport)?.to_vec();
    if (200..300).contains(&status) {
        Ok((headers, body))
    } else {
        Err(http_error(status, &body))
    }
}

fn transport(e: reqwest::Error) -> CloudError {
    CloudError::unavailable(format!("could not reach E2B: {}", e.without_url()))
}

fn envd_transport(e: reqwest::Error) -> CloudError {
    CloudError::unavailable(format!(
        "could not reach the E2B sandbox: {}",
        e.without_url()
    ))
}

/// Map a control-plane status and body to an error. E2B answers `{"code": n, "message": …}`.
pub(crate) fn http_error(status: u16, body: &[u8]) -> CloudError {
    let msg = body_message(body);
    let or = |d: &str| msg.clone().unwrap_or_else(|| d.to_string());
    let account = msg.as_deref().is_some_and(is_account_message);
    match status {
        401 => CloudError::new(
            ErrorKind::NeedsAuth,
            "E2B rejected the API key (vibeke cloud login e2b)",
        ),
        403 if account => CloudError::new(ErrorKind::Account, or("")),
        403 => CloudError::new(
            ErrorKind::NeedsAuth,
            match &msg {
                Some(m) => format!("E2B refused the API key: {m}"),
                None => "E2B refused the API key (vibeke cloud login e2b)".into(),
            },
        ),
        402 => CloudError::new(ErrorKind::Account, or("the E2B account needs billing")),
        404 => CloudError::not_found(or("not found on E2B")),
        409 => CloudError::new(ErrorKind::Conflict, or("E2B reported a conflict")),
        429 if account => CloudError::new(ErrorKind::Account, or("")),
        429 => CloudError::new(ErrorKind::RateLimited, or("E2B rate limit; try again")),
        400 | 422 if account => CloudError::new(ErrorKind::Account, or("")),
        400 | 422 => CloudError::new(ErrorKind::InvalidParams, or("E2B rejected the request")),
        500.. => CloudError::unavailable(or(&format!("E2B returned HTTP {status}"))),
        _ => CloudError::internal(or(&format!("E2B returned HTTP {status}"))),
    }
}

fn is_account_message(m: &str) -> bool {
    let m = m.to_ascii_lowercase();
    [
        "credit card",
        "billing",
        "payment",
        "subscription",
        "upgrade",
        "plan",
        "quota",
        "concurrent",
        "trial",
        "tier",
        "credits",
    ]
    .iter()
    .any(|k| m.contains(k))
}

/// `{"message": …}`, `{"error": …}` or short plain text, at most 300 bytes.
fn body_message(body: &[u8]) -> Option<String> {
    let s = match serde_json::from_slice::<Value>(body) {
        Ok(v) => ["message", "error", "detail"]
            .iter()
            .find_map(|k| match v.get(*k)? {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("message").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })?,
        Err(_) => {
            let t = String::from_utf8_lossy(body).trim().to_string();
            if t.is_empty() || t.len() > 300 || t.starts_with('<') {
                return None;
            }
            t
        }
    };
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut end = s.len().min(300);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    Some(s[..end].to_string())
}

/// A Connect error code (`not_found`, `unavailable`, …) as a [`CloudError`].
pub(crate) fn rpc_error(code: &str, message: &str) -> CloudError {
    let message = if message.is_empty() {
        format!("the E2B sandbox returned {code}")
    } else {
        format!("the E2B sandbox: {message}")
    };
    let kind = match code {
        "unauthenticated" | "permission_denied" => ErrorKind::NeedsAuth,
        "not_found" => ErrorKind::NotFound,
        "already_exists" => ErrorKind::Conflict,
        "invalid_argument" | "failed_precondition" | "out_of_range" => ErrorKind::InvalidParams,
        "resource_exhausted" => ErrorKind::RateLimited,
        "unimplemented" => ErrorKind::Unsupported,
        "unavailable" | "deadline_exceeded" | "canceled" | "aborted" => ErrorKind::Unavailable,
        _ => ErrorKind::Internal,
    };
    CloudError::new(kind, message)
}

/// An envd HTTP error: a Connect error body (`{"code": "not_found", …}`), or a proxy or files
/// error mapped by status.
pub(crate) fn envd_http_error(status: u16, body: &[u8]) -> CloudError {
    if let Ok(v) = serde_json::from_slice::<Value>(body)
        && let Some(code) = v.get("code").and_then(Value::as_str)
    {
        return rpc_error(code, v.get("message").and_then(Value::as_str).unwrap_or(""));
    }
    let msg = body_message(body);
    let or = |d: &str| msg.clone().unwrap_or_else(|| d.to_string());
    match status {
        401 | 403 => CloudError::new(
            ErrorKind::NeedsAuth,
            or("the E2B sandbox refused the access token"),
        ),
        404 => CloudError::not_found(or("not found in the E2B sandbox")),
        429 => CloudError::new(ErrorKind::RateLimited, or("E2B rate limit; try again")),
        400 => CloudError::new(
            ErrorKind::InvalidParams,
            or("the E2B sandbox rejected the request"),
        ),
        507 => CloudError::new(
            ErrorKind::Account,
            or("the E2B sandbox is out of disk space"),
        ),
        _ => CloudError::unavailable(or(&format!("the E2B sandbox returned HTTP {status}"))),
    }
}

fn state_of(s: Option<&str>) -> BoxState {
    match s {
        Some("running") => BoxState::Running,
        Some("paused" | "pausing") => BoxState::Paused,
        Some("killed" | "stopped") => BoxState::Stopped,
        _ => BoxState::Unknown,
    }
}

/// Owner tags from sandbox metadata, when both are well-formed.
fn tags_from(meta: Option<&Value>) -> Option<naming::Tags> {
    let m = meta?;
    let host = m.get("vibeke_host")?.as_str()?;
    let key = m.get("vibeke_key")?.as_str()?;
    naming::parse_name(&format!("{}{host}-{key}", naming::PREFIX))
}

/// A `ListedSandbox`, `SandboxDetail` or `Sandbox` as a box.
pub(crate) fn parse_box(v: &Value) -> Option<RemoteBox> {
    let id = v.get("sandboxID")?.as_str()?.to_string();
    let meta = v.get("metadata");
    let tags = tags_from(meta);
    let name = meta
        .and_then(|m| m.get("vibeke_name"))
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .or_else(|| tags.as_ref().map(naming::box_name))
        .unwrap_or_else(|| id.clone());
    let created = v
        .get("startedAt")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .unwrap_or(0);
    Some(RemoteBox {
        provider: "e2b".into(),
        id,
        name,
        state: state_of(v.get("state").and_then(Value::as_str)),
        created_at: created,
        last_active_at: created,
        url: None,
        tags,
    })
}

/// One Connect envelope.
pub(crate) fn envelope(flags: u8, msg: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(msg.len() + 5);
    f.push(flags);
    f.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    f.extend_from_slice(msg);
    f
}

/// Splits a Connect response body into envelopes.
#[derive(Default)]
pub(crate) struct Frames {
    buf: Vec<u8>,
}

impl Frames {
    pub(crate) fn push(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// The next complete envelope as `(flags, message)`.
    pub(crate) fn next_frame(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[1], self.buf[2], self.buf[3], self.buf[4]]) as usize;
        if len > MAX_FRAME {
            return Err(CloudError::unavailable(
                "the E2B sandbox sent an oversized message",
            ));
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let flags = self.buf[0];
        let msg = self.buf[5..5 + len].to_vec();
        self.buf = self.buf.split_off(5 + len);
        if flags & FLAG_COMPRESSED != 0 {
            return Err(CloudError::unavailable(
                "the E2B sandbox sent a compressed message",
            ));
        }
        Ok(Some((flags, msg)))
    }
}

/// The error in an end-of-stream message, if any.
pub(crate) fn end_stream_error(v: &Value) -> Option<CloudError> {
    let e = v.get("error").filter(|e| e.is_object())?;
    Some(rpc_error(
        e.get("code").and_then(Value::as_str).unwrap_or("unknown"),
        e.get("message").and_then(Value::as_str).unwrap_or(""),
    ))
}

pub(crate) fn b64(d: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(d)
}

/// Decode protobuf JSON bytes: standard or URL-safe alphabet, with or without padding.
pub(crate) fn unb64(s: &str) -> Vec<u8> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    for e in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
        if let Ok(d) = e.decode(s) {
            return d;
        }
    }
    Vec::new()
}

/// One `process.ProcessEvent`.
#[derive(Debug, PartialEq)]
pub(crate) enum Event {
    Start(u32),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    End(i32),
    Other,
}

fn num_u32(v: Option<&Value>) -> Option<u32> {
    match v? {
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// A `StartResponse`/`ConnectResponse` (`{"event": {…}}`) as an [`Event`].
pub(crate) fn parse_event(v: &Value) -> Event {
    let ev = v.get("event").unwrap_or(v);
    if let Some(s) = ev.get("start") {
        return match num_u32(s.get("pid")) {
            Some(p) => Event::Start(p),
            None => Event::Other,
        };
    }
    if let Some(d) = ev.get("data") {
        if let Some(s) = d
            .get("pty")
            .or_else(|| d.get("stdout"))
            .and_then(Value::as_str)
        {
            return Event::Stdout(unb64(s));
        }
        if let Some(s) = d.get("stderr").and_then(Value::as_str) {
            return Event::Stderr(unb64(s));
        }
        return Event::Other;
    }
    if let Some(e) = ev.get("end") {
        return Event::End(end_code(e));
    }
    Event::Other
}

/// Exit code of an `EndEvent`. Protobuf JSON leaves out zero values, so a missing `exitCode`
/// is 0 unless the status says a signal ended the process.
fn end_code(e: &Value) -> i32 {
    if let Some(c) = e
        .get("exitCode")
        .or_else(|| e.get("exit_code"))
        .and_then(Value::as_i64)
    {
        return c as i32;
    }
    let status = e.get("status").and_then(Value::as_str).unwrap_or("");
    if let Some(n) = status
        .strip_prefix("exit status ")
        .and_then(|n| n.trim().parse::<i32>().ok())
    {
        return n;
    }
    match status.strip_prefix("signal: ").map(str::trim) {
        Some("hangup") => 129,
        Some("interrupt") => 130,
        Some("killed") => 137,
        Some("terminated") => 143,
        Some(_) => -1,
        None => 0,
    }
}

/// A process as `List` reports it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Proc {
    pub pid: u32,
    pub command: String,
    pub tag: Option<String>,
}

impl Proc {
    /// Whether the process has a terminal, from the tag Vibeke gave it; `None` when unknown.
    fn tty(&self) -> Option<bool> {
        let t = self.tag.as_deref()?;
        if t.starts_with("vk-tty-") {
            Some(true)
        } else if t.starts_with("vk-pipe-") {
            Some(false)
        } else {
            None
        }
    }
}

pub(crate) fn procs_from(v: &Value) -> Vec<Proc> {
    v.get("processes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|p| {
            let pid = num_u32(p.get("pid"))?;
            let cfg = p.get("config");
            let mut parts: Vec<String> = Vec::new();
            if let Some(c) = cfg.and_then(|c| c.get("cmd")).and_then(Value::as_str) {
                parts.push(c.to_string());
            }
            if let Some(a) = cfg.and_then(|c| c.get("args")).and_then(Value::as_array) {
                parts.extend(a.iter().filter_map(Value::as_str).map(str::to_string));
            }
            Some(Proc {
                pid,
                command: parts.join(" "),
                tag: p
                    .get("tag")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string),
            })
        })
        .collect()
}

/// A unique process tag that records whether the process has a terminal.
fn new_tag(tty: bool) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("vk-{}-{t:x}{n:x}", if tty { "tty" } else { "pipe" })
}

/// The `StartRequest` for `req`.
pub(crate) fn start_request(req: &ExecReq, tag: &str) -> Value {
    let envs: Map<String, Value> = req
        .env
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    let mut process = json!({
        "cmd": req.argv[0],
        "args": &req.argv[1..],
        "envs": envs,
    });
    if let Some(d) = req.cwd.as_deref().filter(|d| !d.is_empty()) {
        process["cwd"] = json!(d);
    }
    let mut body = json!({"process": process, "tag": tag});
    if req.tty {
        body["pty"] = json!({"size": {"cols": req.cols.max(1), "rows": req.rows.max(1)}});
    } else {
        body["stdin"] = json!(true);
    }
    body
}

/// The unary request for one input on process `pid`; `None` when it has no envd call.
pub(crate) fn input_request(pid: u32, tty: bool, i: &In) -> Option<(&'static str, Value)> {
    let sel = json!({"pid": pid});
    match i {
        In::Data(d) if d.is_empty() => None,
        In::Data(d) => {
            let key = if tty { "pty" } else { "stdin" };
            Some(("SendInput", json!({"process": sel, "input": {key: b64(d)}})))
        }
        // A terminal has no separate stdin to close.
        In::Eof if tty => None,
        In::Eof => Some(("CloseStdin", json!({"process": sel}))),
        In::Resize { .. } if !tty => None,
        In::Resize { cols, rows } => Some((
            "Update",
            json!({"process": sel, "pty": {"size": {"cols": (*cols).max(1), "rows": (*rows).max(1)}}}),
        )),
        In::Signal(s) => {
            let s = s.trim().to_ascii_uppercase();
            let s = s.trim_start_matches("SIG");
            let sig = if s == "KILL" || s == "9" {
                "SIGNAL_SIGKILL"
            } else {
                "SIGNAL_SIGTERM"
            };
            Some(("SendSignal", json!({"process": sel, "signal": sig})))
        }
    }
}

/// envd in one sandbox.
#[derive(Clone)]
struct Envd {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
    sandbox: String,
}

impl Envd {
    fn post(&self, url: &str) -> reqwest::RequestBuilder {
        let mut rb = self
            .http
            .post(url)
            .header("E2b-Sandbox-Id", &self.sandbox)
            .header("E2b-Sandbox-Port", ENVD_PORT.to_string());
        if let Some(t) = &self.token {
            rb = rb.header("X-Access-Token", t);
        }
        rb
    }

    fn rpc_url(&self, method: &str) -> String {
        format!("{}/process.Process/{method}", self.base)
    }

    /// A unary Connect call with JSON.
    async fn unary(&self, method: &str, body: &Value) -> Result<Value> {
        let resp = self
            .post(&self.rpc_url(method))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("Connect-Protocol-Version", "1")
            .body(body.to_string())
            .timeout(UNARY_TIMEOUT)
            .send()
            .await
            .map_err(envd_transport)?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(envd_transport)?;
        if !(200..300).contains(&status) {
            return Err(envd_http_error(status, &bytes));
        }
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Open a server-streaming Connect call; the response body carries envelopes.
    async fn stream(&self, method: &str, body: &Value) -> Result<reqwest::Response> {
        let auth = format!("Basic {}", b64(format!("{USER}:").as_bytes()));
        let resp = self
            .post(&self.rpc_url(method))
            .header(reqwest::header::CONTENT_TYPE, "application/connect+json")
            .header("Connect-Protocol-Version", "1")
            .header(reqwest::header::AUTHORIZATION, auth)
            .header("Keepalive-Ping-Interval", KEEPALIVE_S.to_string())
            .body(envelope(0, body.to_string().as_bytes()))
            .send()
            .await
            .map_err(envd_transport)?;
        let status = resp.status().as_u16();
        if status != 200 {
            let bytes = resp.bytes().await.unwrap_or_default();
            return Err(envd_http_error(status, &bytes));
        }
        Ok(resp)
    }

    /// `POST /files?path=&username=` with one multipart part named `file`. envd creates the
    /// parent directories.
    async fn upload(&self, path: &str, data: &[u8]) -> Result<()> {
        let mut url = Url::parse(&format!("{}/files", self.base))
            .map_err(|e| CloudError::internal(format!("bad envd url: {e}")))?;
        url.query_pairs_mut()
            .append_pair("path", path)
            .append_pair("username", USER);
        let boundary = format!("vibeke-{}", new_tag(false));
        let fname: String = path
            .chars()
            .filter(|c| !matches!(c, '\r' | '\n'))
            .map(|c| if c == '"' { '_' } else { c })
            .collect();
        let mut body = Vec::with_capacity(data.len() + 256);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{fname}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let resp = self
            .post(url.as_str())
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .map_err(envd_transport)?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(envd_transport)?;
        if !(200..300).contains(&status) {
            return Err(envd_http_error(status, &bytes));
        }
        Ok(())
    }
}

/// Open a process stream and read up to its start event: the response, the frames read past
/// it, the pid and any output that came first.
async fn handshake(
    envd: &Envd,
    method: &str,
    body: &Value,
) -> Result<(reqwest::Response, Frames, u32, Vec<Out>)> {
    let mut resp = envd.stream(method, body).await?;
    let mut frames = Frames::default();
    let mut pending = Vec::new();
    let deadline = tokio::time::Instant::now() + START_WAIT;
    loop {
        while let Some((flags, msg)) = frames.next_frame()? {
            let v: Value = serde_json::from_slice(&msg).unwrap_or(Value::Null);
            if flags & FLAG_END_STREAM != 0 {
                return Err(end_stream_error(&v).unwrap_or_else(|| {
                    CloudError::unavailable("the E2B process ended before it started")
                }));
            }
            match parse_event(&v) {
                Event::Start(pid) => return Ok((resp, frames, pid, pending)),
                Event::Stdout(d) => pending.push(Out::Stdout(d)),
                Event::Stderr(d) => pending.push(Out::Stderr(d)),
                Event::End(_) | Event::Other => {}
            }
        }
        match tokio::time::timeout_at(deadline, resp.chunk()).await {
            Err(_) => {
                return Err(CloudError::unavailable(
                    "timed out waiting for the E2B process to start",
                ));
            }
            Ok(Err(e)) => return Err(envd_transport(e)),
            Ok(Ok(None)) => {
                return Err(CloudError::unavailable(
                    "the E2B sandbox closed the stream before the process started",
                ));
            }
            Ok(Ok(Some(b))) => frames.push(&b),
        }
    }
}

/// What the bridge needs to send calls for one process.
struct Ctl {
    envd: Envd,
    pid: u32,
    tty: bool,
    detachable: bool,
}

struct Pump {
    ctl: Ctl,
    resp: reqwest::Response,
    frames: Frames,
    pending: Vec<Out>,
}

enum Flow {
    Continue,
    /// The process exited.
    Done,
    /// The client dropped the output.
    Gone,
    /// The stream ended without an exit.
    Lost(String),
}

/// Deliver every complete frame in the buffer.
async fn drain(frames: &mut Frames, out: &mpsc::Sender<Out>) -> Flow {
    loop {
        let (flags, msg) = match frames.next_frame() {
            Ok(Some(f)) => f,
            Ok(None) => return Flow::Continue,
            Err(e) => return Flow::Lost(e.message),
        };
        let v: Value = serde_json::from_slice(&msg).unwrap_or(Value::Null);
        if flags & FLAG_END_STREAM != 0 {
            return Flow::Lost(match end_stream_error(&v) {
                Some(e) => e.message,
                None => "the E2B process stream ended".into(),
            });
        }
        let o = match parse_event(&v) {
            Event::Stdout(d) => Out::Stdout(d),
            Event::Stderr(d) => Out::Stderr(d),
            Event::End(c) => {
                let _ = out.send(Out::Exit(c)).await;
                return Flow::Done;
            }
            Event::Start(_) | Event::Other => continue,
        };
        if out.send(o).await.is_err() {
            return Flow::Gone;
        }
    }
}

/// Send one input (merging queued data); returns an input taken from the queue that still
/// needs sending.
async fn apply(p: &Ctl, i: In, input: &mut mpsc::Receiver<In>) -> Option<In> {
    let mut next = None;
    let i = match i {
        In::Data(mut d) => {
            while d.len() < COALESCE {
                match input.try_recv() {
                    Ok(In::Data(more)) => d.extend_from_slice(&more),
                    Ok(other) => {
                        next = Some(other);
                        break;
                    }
                    Err(_) => break,
                }
            }
            In::Data(d)
        }
        other => other,
    };
    if let Some((method, body)) = input_request(p.pid, p.tty, &i)
        && let Err(e) = p.envd.unary(method, &body).await
    {
        // A finished process answers not_found; its stream reports the exit.
        tracing::debug!(method, error = %e, "e2b input call failed");
    }
    next
}

/// Kill a session nobody can attach to again once its client is gone.
async fn kill_if_owned(p: &Ctl) {
    if !p.detachable
        && let Some((m, b)) = input_request(p.pid, p.tty, &In::Signal("KILL".into()))
    {
        let _ = p.envd.unary(m, &b).await;
    }
}

/// Bridge the process stream and the session channels until exit, loss or detach.
async fn pump(p: Pump, mut input: mpsc::Receiver<In>, out: mpsc::Sender<Out>) {
    let Pump {
        ctl,
        mut resp,
        mut frames,
        pending,
    } = p;
    for o in pending {
        if out.send(o).await.is_err() {
            kill_if_owned(&ctl).await;
            return;
        }
    }
    let mut flow = drain(&mut frames, &out).await;
    loop {
        match flow {
            Flow::Continue => {}
            Flow::Done => return,
            Flow::Gone => {
                kill_if_owned(&ctl).await;
                return;
            }
            Flow::Lost(m) => {
                let _ = out.send(Out::Lost(m)).await;
                kill_if_owned(&ctl).await;
                return;
            }
        }
        flow = tokio::select! {
            i = input.recv() => match i {
                // Input dropped: detach. A detachable process keeps running.
                None => {
                    kill_if_owned(&ctl).await;
                    return;
                }
                Some(i) => {
                    let mut held = apply(&ctl, i, &mut input).await;
                    while let Some(h) = held.take() {
                        held = apply(&ctl, h, &mut input).await;
                    }
                    Flow::Continue
                }
            },
            c = tokio::time::timeout(IDLE_LIMIT, resp.chunk()) => match c {
                Err(_) => Flow::Lost("no data or keepalive from the E2B sandbox".into()),
                Ok(Err(e)) => Flow::Lost(format!("connection lost: {}", e.without_url())),
                Ok(Ok(None)) => Flow::Lost("connection closed".into()),
                Ok(Ok(Some(b))) => {
                    frames.push(&b);
                    drain(&mut frames, &out).await
                }
            },
            _ = out.closed() => {
                kill_if_owned(&ctl).await;
                return;
            }
        };
    }
}

/// The API key in the e2b CLI's `~/.e2b/config.json` (v2 `projectApiKey`, v1 `teamApiKey`).
pub(crate) fn e2b_cli_key(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    ["projectApiKey", "teamApiKey", "apiKey"]
        .iter()
        .find_map(|k| {
            v.get(*k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

impl Provider for E2b {
    fn id(&self) -> &'static str {
        "e2b"
    }
    fn label(&self) -> &'static str {
        "E2B"
    }
    fn caps(&self) -> Caps {
        Caps {
            resize: true,
            reattach: true,
            explicit_suspend: true,
            keeps_memory: true,
            checkpoints: true,
            port_urls: true,
            max_runtime_s: MAX_RUNTIME_S,
        }
    }
    fn auth_methods(&self) -> Vec<AuthMethod> {
        vec![
            AuthMethod::PasteToken {
                label: "E2B API key".into(),
                help_url: "https://e2b.dev/dashboard?tab=keys".into(),
                hint: "Starts with e2b_".into(),
            },
            AuthMethod::Import {
                source: "e2b-cli".into(),
                label: "Use the e2b CLI login".into(),
            },
            AuthMethod::Env {
                var: "E2B_API_KEY".into(),
            },
        ]
    }

    fn verify<'a>(&'a self, cred: &'a Secret) -> BoxFut<'a, Result<Account>> {
        Box::pin(async move {
            // The API key can't read the team name (`GET /teams` takes a user token).
            let url = self.url(&["v2", "sandboxes"], &[("limit", "1".into())])?;
            send(self.req(cred, reqwest::Method::GET, url)?).await?;
            Ok(Account {
                label: "E2B".into(),
                details: Default::default(),
            })
        })
    }

    fn import<'a>(&'a self, source: &'a str) -> BoxFut<'a, Result<Option<Secret>>> {
        Box::pin(async move {
            match source {
                "e2b-cli" => {
                    let path = crate::config::expand_home("~/.e2b/config.json");
                    let Ok(text) = tokio::fs::read_to_string(&path).await else {
                        return Ok(None);
                    };
                    Ok(e2b_cli_key(&text).map(Secret::new))
                }
                _ => Err(CloudError::new(
                    ErrorKind::InvalidParams,
                    format!("unknown import source {source:?} for e2b"),
                )),
            }
        })
    }

    fn create<'a>(
        &'a self,
        cred: &'a Secret,
        spec: &'a CreateSpec,
    ) -> BoxFut<'a, Result<RemoteBox>> {
        Box::pin(async move {
            let template = spec
                .template
                .as_deref()
                .or(self.cfg.template.as_deref())
                .filter(|t| !t.trim().is_empty())
                .unwrap_or(DEFAULT_TEMPLATE);
            let env: Map<String, Value> = spec
                .env
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect();
            let body = json!({
                "templateID": template,
                "timeout": self.timeout_s(spec.timeout_s),
                "autoPause": spec.auto_pause || self.cfg.auto_pause.unwrap_or(true),
                "metadata": {
                    "vibeke_host": spec.tags.host,
                    "vibeke_key": spec.tags.key,
                    "vibeke_name": spec.name,
                },
                "envVars": env,
            });
            let url = self.url(&["v2", "sandboxes"], &[])?;
            let (_, resp) = send(self.post_json(cred, url, &body)?).await?;
            let v: Value = serde_json::from_slice(&resp)
                .map_err(|e| CloudError::unavailable(format!("unexpected E2B response: {e}")))?;
            let id = v
                .get("sandboxID")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| CloudError::unavailable("E2B did not return a sandbox id"))?
                .to_string();
            self.remember(&id, &v);
            Ok(RemoteBox {
                provider: "e2b".into(),
                id,
                name: spec.name.clone(),
                state: BoxState::Running,
                created_at: crate::now_s(),
                last_active_at: crate::now_s(),
                url: None,
                tags: Some(spec.tags.clone()),
            })
        })
    }

    fn get<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<RemoteBox>> {
        Box::pin(self.get_box(cred, id))
    }

    fn list<'a>(&'a self, cred: &'a Secret) -> BoxFut<'a, Result<Vec<RemoteBox>>> {
        Box::pin(async move {
            // Without a state filter E2B lists running and paused sandboxes. Metadata filters
            // match values, not key presence, so Vibeke boxes are picked here.
            let mut out = Vec::new();
            let mut token: Option<String> = None;
            for _ in 0..100 {
                let mut q = vec![("limit", "100".to_string())];
                if let Some(t) = &token {
                    q.push(("nextToken", t.clone()));
                }
                let url = self.url(&["v2", "sandboxes"], &q)?;
                let (headers, body) = send(self.req(cred, reqwest::Method::GET, url)?).await?;
                let v: Value = serde_json::from_slice(&body).map_err(|e| {
                    CloudError::unavailable(format!("unexpected E2B response: {e}"))
                })?;
                out.extend(
                    v.as_array()
                        .map(Vec::as_slice)
                        .unwrap_or_default()
                        .iter()
                        .filter_map(parse_box)
                        .filter(|b| b.tags.is_some()),
                );
                token = headers
                    .get("x-next-token")
                    .and_then(|h| h.to_str().ok())
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string);
                if token.is_none() {
                    break;
                }
            }
            Ok(out)
        })
    }

    fn destroy<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            let url = self.url(&["sandboxes", id], &[])?;
            let r = send(self.req(cred, reqwest::Method::DELETE, url)?).await;
            self.forget(id);
            match r {
                Ok(_) => Ok(()),
                Err(e) if e.kind == ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        })
    }

    fn suspend<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            let url = self.url(&["sandboxes", id, "pause"], &[])?;
            match send(self.post_json(cred, url, &json!({}))?).await {
                Ok(_) => Ok(()),
                // Already paused.
                Err(e) if e.kind == ErrorKind::Conflict => Ok(()),
                Err(e) => Err(e),
            }
        })
    }

    fn resume<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<()>> {
        Box::pin(async move { self.connect_box(cred, id).await.map(|_| ()) })
    }

    fn checkpoint<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        _note: &'a str,
    ) -> BoxFut<'a, Result<String>> {
        Box::pin(async move {
            // A snapshot is a template new sandboxes can start from; the sandbox keeps running.
            // Snapshot names are template aliases, so the free-text note is not one.
            let url = self.url(&["sandboxes", id, "snapshots"], &[])?;
            let (_, body) = send(
                self.post_json(cred, url, &json!({}))?
                    .timeout(Duration::from_secs(600)),
            )
            .await?;
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            v.get("snapshotID")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| CloudError::unavailable("E2B did not return a snapshot id"))
        })
    }

    fn exec<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        req: ExecReq,
    ) -> BoxFut<'a, Result<Session>> {
        Box::pin(async move {
            if req.argv.is_empty() || req.argv[0].is_empty() {
                return Err(CloudError::new(ErrorKind::InvalidParams, "empty command"));
            }
            let body = start_request(&req, &new_tag(req.tty));
            let (_, s) = self
                .open(cred, id, "Start", body, req.tty, req.detachable)
                .await?;
            Ok(s)
        })
    }

    fn attach<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        session: &'a str,
        cols: u16,
        rows: u16,
    ) -> BoxFut<'a, Result<Session>> {
        Box::pin(async move {
            let pid: u32 = session
                .trim()
                .parse()
                .map_err(|_| CloudError::not_found("the session has ended"))?;
            let procs = self.processes(cred, id).await?;
            let Some(proc_) = procs.iter().find(|p| p.pid == pid) else {
                return Err(CloudError::not_found("the session has ended"));
            };
            // Sessions without a Vibeke tag are assumed to be terminals (panes attach).
            let tty = proc_.tty().unwrap_or(true);
            let body = json!({"process": {"pid": pid}});
            let (envd, s) = self.open(cred, id, "Connect", body, tty, true).await?;
            if tty
                && cols > 0
                && rows > 0
                && let Some((m, b)) = input_request(pid, true, &In::Resize { cols, rows })
                && let Err(e) = envd.unary(m, &b).await
            {
                tracing::debug!(error = %e, "e2b attach resize failed");
            }
            Ok(s)
        })
    }

    fn sessions<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
    ) -> BoxFut<'a, Result<Vec<SessionInfo>>> {
        Box::pin(async move {
            Ok(self
                .processes(cred, id)
                .await?
                .into_iter()
                .map(|p| SessionInfo {
                    id: p.pid.to_string(),
                    tty: p.tty().unwrap_or(false),
                    command: p.command,
                    active: true,
                    last_activity_at: 0,
                })
                .collect())
        })
    }

    fn write_file<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        path: &'a str,
        data: Vec<u8>,
        mode: u32,
    ) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            if path.is_empty() {
                return Err(CloudError::new(ErrorKind::InvalidParams, "empty path"));
            }
            let data = &data;
            self.with_envd(cred, id, |e| async move { e.upload(path, data).await })
                .await?;
            let mode = mode & 0o7777;
            if mode != 0o644 {
                let (code, err) = self
                    .run_quiet(
                        cred,
                        id,
                        vec!["chmod".into(), format!("{mode:o}"), path.to_string()],
                    )
                    .await?;
                if code != 0 {
                    return Err(CloudError::internal(format!(
                        "chmod {mode:o} failed in the E2B sandbox (exit {code}): {err}"
                    )));
                }
            }
            Ok(())
        })
    }

    fn port_url<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        port: u16,
    ) -> BoxFut<'a, Result<Option<String>>> {
        Box::pin(async move {
            let domain = match self.cached(id) {
                Some(a) => a.domain,
                None => {
                    self.get_box(cred, id).await?;
                    self.cached(id)
                        .map(|a| a.domain)
                        .unwrap_or_else(|| self.default_domain())
                }
            };
            Ok(Some(format!("https://{port}-{id}.{domain}")))
        })
    }
}

#[cfg(test)]
#[path = "e2b_tests.rs"]
mod tests;
