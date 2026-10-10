//! Fly.io Sprites (spec 17 §3.3). A Sprite is a persistent Linux box that sleeps when idle and
//! wakes on the next request, so suspend and resume are no-ops. The REST API manages boxes and
//! files; commands run over a WebSocket per exec session.
//!
//! - Boxes: `POST /sprites`, `GET /sprites?prefix=vk-` (continuation tokens),
//!   `GET`/`DELETE /sprites/{name}`. The provider id of a box is its name.
//! - Exec: `wss …/sprites/{name}/exec?cmd=…&path=…&tty=…` and attach on
//!   `…/exec/{session_id}`. In terminal mode binary frames are raw bytes and text frames are
//!   JSON control messages; in pipe mode every binary frame starts with a stream byte
//!   ([`stream`]).
//! - Auth: `Authorization: Bearer <org>/<id>/<secret>`.

use std::process::Stdio;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, Message};
use url::Url;

use crate::{
    Account, AuthMethod, BoxFut, BoxState, Caps, CloudError, CreateSpec, ErrorKind, ExecReq, In,
    Out, Provider, ProviderConfig, RemoteBox, Result, Secret, Session, SessionInfo, naming,
};

pub const DEFAULT_API: &str = "https://api.sprites.dev/v1";
/// The only port a Sprite's URL routes to.
pub const HTTP_PORT: u16 = 8080;

/// Pipe-mode stream bytes (first byte of every binary frame).
pub mod stream {
    pub const STDIN: u8 = 0x00;
    pub const STDOUT: u8 = 0x01;
    pub const STDERR: u8 = 0x02;
    pub const EXIT: u8 = 0x03;
    pub const STDIN_EOF: u8 = 0x04;
}

const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a new or attached session waits for `session_info` before going on without an id.
const INFO_WAIT: Duration = Duration::from_secs(5);

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub struct Sprites {
    pub cfg: ProviderConfig,
    http: reqwest::Client,
}

impl Sprites {
    pub fn new(cfg: ProviderConfig) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .user_agent(concat!("vibeke/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default();
        Sprites { cfg, http }
    }

    fn base(&self) -> &str {
        self.cfg
            .api_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(DEFAULT_API)
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

    fn ws_url(&self, segs: &[&str], query: &[(&str, String)]) -> Result<Url> {
        let mut u = self.url(segs, query)?;
        let scheme = if u.scheme() == "http" { "ws" } else { "wss" };
        u.set_scheme(scheme)
            .map_err(|_| CloudError::new(ErrorKind::InvalidParams, "bad api_url scheme"))?;
        Ok(u)
    }

    fn req(&self, cred: &Secret, method: reqwest::Method, url: Url) -> reqwest::RequestBuilder {
        self.http
            .request(method, url)
            .bearer_auth(cred.expose())
            .timeout(HTTP_TIMEOUT)
    }

    async fn get_box(&self, cred: &Secret, id: &str) -> Result<RemoteBox> {
        let body =
            send(self.req(cred, reqwest::Method::GET, self.url(&["sprites", id], &[])?)).await?;
        let v: Value = serde_json::from_slice(&body)
            .map_err(|e| CloudError::unavailable(format!("unexpected Sprites response: {e}")))?;
        parse_box(&v).ok_or_else(|| CloudError::unavailable("unexpected Sprites response"))
    }

    async fn open_ws(&self, cred: &Secret, url: Url) -> Result<Ws> {
        use tungstenite::client::IntoClientRequest;
        use tungstenite::http::{HeaderValue, header};
        let mut req = url
            .as_str()
            .into_client_request()
            .map_err(CloudError::internal)?;
        let v = HeaderValue::from_str(&format!("Bearer {}", cred.expose())).map_err(|_| {
            CloudError::new(
                ErrorKind::NeedsAuth,
                "the Sprites token contains characters a header can't carry",
            )
        })?;
        req.headers_mut().insert(header::AUTHORIZATION, v);
        match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(req)).await {
            Err(_) => Err(CloudError::unavailable(
                "timed out connecting to the Sprites exec service",
            )),
            Ok(Err(tungstenite::Error::Http(resp))) => Err(http_error(
                resp.status().as_u16(),
                resp.body().as_deref().unwrap_or_default(),
            )),
            Ok(Err(e)) => Err(CloudError::unavailable(format!(
                "could not connect to the Sprites exec service: {e}"
            ))),
            Ok(Ok((ws, _))) => Ok(ws),
        }
    }

    /// Open an exec or attach socket and start the bridge task.
    async fn start(&self, cred: &Secret, url: Url, tty: bool, wait_info: bool) -> Result<Session> {
        let mut ws = self.open_ws(cred, url).await?;
        let mut id = String::new();
        let mut pending = None;
        if wait_info {
            match tokio::time::timeout(INFO_WAIT, ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => match parse_ctl(t.as_str()) {
                    Ctl::SessionInfo(i) => id = i,
                    _ => pending = Some(Message::Text(t)),
                },
                Ok(Some(Ok(m))) => pending = Some(m),
                Ok(Some(Err(e))) => {
                    return Err(CloudError::unavailable(format!(
                        "the Sprites exec session failed: {e}"
                    )));
                }
                Ok(None) => {
                    return Err(CloudError::unavailable(
                        "Sprites closed the exec session at once",
                    ));
                }
                Err(_) => {}
            }
        }
        let (in_tx, in_rx) = mpsc::channel(64);
        let (out_tx, out_rx) = mpsc::channel(256);
        tokio::spawn(pump(ws, tty, pending, in_rx, out_tx));
        Ok(Session {
            id,
            tty,
            input: in_tx,
            output: out_rx,
        })
    }

    async fn import_fly(&self) -> Result<Option<Secret>> {
        let Some(token) = run_fly(&["auth", "token"]).await else {
            return Ok(None);
        };
        let token = token.trim().to_string();
        if token.is_empty() {
            return Ok(None);
        }
        let Some(orgs) = run_fly(&["orgs", "list", "--json"]).await else {
            return Ok(None);
        };
        let Some(org) = pick_fly_org(&orgs) else {
            return Err(CloudError::new(
                ErrorKind::Account,
                "fly.io lists no organization for this login",
            ));
        };
        let auth = if token.starts_with("FlyV1 ") {
            token
        } else {
            format!("FlyV1 {token}")
        };
        let url = self.url(&["organizations", &org, "tokens"], &[])?;
        let body = send(
            self.http
                .post(url)
                .header(reqwest::header::AUTHORIZATION, auth)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(json!({"description": "vibeke"}).to_string())
                .timeout(HTTP_TIMEOUT),
        )
        .await?;
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        match v.get("token").and_then(Value::as_str) {
            Some(t) if !t.trim().is_empty() => Ok(Some(Secret::new(t))),
            _ => Err(CloudError::unavailable(
                "Sprites did not return a token for the fly.io login",
            )),
        }
    }
}

/// Send a request; a non-2xx status becomes a [`CloudError`].
async fn send(rb: reqwest::RequestBuilder) -> Result<Vec<u8>> {
    let resp = rb.send().await.map_err(transport)?;
    let status = resp.status().as_u16();
    let body = resp.bytes().await.map_err(transport)?.to_vec();
    if (200..300).contains(&status) {
        Ok(body)
    } else {
        Err(http_error(status, &body))
    }
}

fn transport(e: reqwest::Error) -> CloudError {
    // Without the URL: it is not secret, but it is noise in the client.
    CloudError::unavailable(format!("could not reach Sprites: {}", e.without_url()))
}

/// Map an HTTP status and body to an error. 403 is a credential problem unless the provider
/// says the account can't do this yet (billing, plan, quota).
pub(crate) fn http_error(status: u16, body: &[u8]) -> CloudError {
    let msg = body_message(body);
    let or = |d: &str| msg.clone().unwrap_or_else(|| d.to_string());
    match status {
        401 => CloudError::new(
            ErrorKind::NeedsAuth,
            "Sprites rejected the token (vibeke cloud login sprites)",
        ),
        403 => match &msg {
            Some(m) if is_account_message(m) => CloudError::new(ErrorKind::Account, m.clone()),
            Some(m) => CloudError::new(
                ErrorKind::NeedsAuth,
                format!("Sprites refused the token: {m}"),
            ),
            None => CloudError::new(
                ErrorKind::NeedsAuth,
                "Sprites refused the token (vibeke cloud login sprites)",
            ),
        },
        402 => CloudError::new(ErrorKind::Account, or("the Sprites account needs billing")),
        404 => CloudError::not_found(or("not found on Sprites")),
        409 => CloudError::new(ErrorKind::Conflict, or("already exists on Sprites")),
        429 => CloudError::new(ErrorKind::RateLimited, or("Sprites rate limit; try again")),
        400 | 422 => CloudError::new(ErrorKind::InvalidParams, or("Sprites rejected the request")),
        500.. => CloudError::unavailable(or(&format!("Sprites returned HTTP {status}"))),
        _ => CloudError::internal(or(&format!("Sprites returned HTTP {status}"))),
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
        "limit",
        "trial",
    ]
    .iter()
    .any(|k| m.contains(k))
}

/// `{"error": "..."}`, `{"error": {"message": ...}}`, `{"message": ...}` or short plain text.
fn body_message(body: &[u8]) -> Option<String> {
    let s = if let Ok(v) = serde_json::from_slice::<Value>(body) {
        let pick = |v: &Value| -> Option<String> {
            match v {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("message").and_then(Value::as_str).map(str::to_string),
                _ => None,
            }
        };
        ["error", "message", "detail", "errors"]
            .iter()
            .find_map(|k| v.get(*k).and_then(pick))?
    } else {
        let t = String::from_utf8_lossy(body).trim().to_string();
        if t.is_empty() || t.len() > 300 || t.starts_with('<') {
            return None;
        }
        t
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

fn state_of(s: Option<&str>) -> BoxState {
    match s {
        Some("running") => BoxState::Running,
        Some("warm") => BoxState::Warm,
        Some("cold") => BoxState::Cold,
        Some("creating" | "provisioning" | "pending") => BoxState::Creating,
        Some("stopped" | "destroyed") => BoxState::Stopped,
        _ => BoxState::Unknown,
    }
}

/// A `SpriteResponse` as a box.
pub(crate) fn parse_box(v: &Value) -> Option<RemoteBox> {
    let name = v.get("name")?.as_str()?.to_string();
    let last = ts(v.get("last_running_at")).max(ts(v.get("last_warming_at")));
    Some(RemoteBox {
        provider: "sprites".into(),
        id: name.clone(),
        state: state_of(v.get("status").and_then(Value::as_str)),
        created_at: ts(v.get("created_at")),
        last_active_at: last,
        url: v
            .get("url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .map(str::to_string),
        tags: naming::parse_name(&name),
        name,
    })
}

/// A timestamp field as unix seconds: ISO 8601 text or a number (seconds or milliseconds).
fn ts(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::String(s)) => parse_rfc3339(s).unwrap_or(0),
        Some(Value::Number(n)) => {
            let n = n.as_u64().unwrap_or(0);
            if n > 100_000_000_000 { n / 1000 } else { n }
        }
        _ => 0,
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)` to unix seconds.
pub(crate) fn parse_rfc3339(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        if !t.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        t.parse::<i64>().ok()
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        while b.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
    }
    let off = match b.get(i) {
        None | Some(b'Z' | b'z') => 0,
        Some(c @ (b'+' | b'-')) => {
            let sign = if *c == b'+' { 1 } else { -1 };
            sign * (num(i + 1..i + 3)? * 3600 + num(i + 4..i + 6)? * 60)
        }
        _ => return None,
    };
    let t = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - off;
    u64::try_from(t).ok()
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Control messages from the exec socket.
#[derive(Debug, PartialEq)]
enum Ctl {
    SessionInfo(String),
    Exit(i32),
    Port { port: u16, url: Option<String> },
    Other,
}

fn id_str(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_ctl(t: &str) -> Ctl {
    let Ok(v) = serde_json::from_str::<Value>(t) else {
        return Ctl::Other;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("session_info") => Ctl::SessionInfo(
            id_str(v.get("session_id"))
                .or_else(|| id_str(v.get("id")))
                .unwrap_or_default(),
        ),
        Some("exit") => Ctl::Exit(
            v.get("exit_code")
                .or_else(|| v.get("code"))
                .and_then(Value::as_i64)
                .unwrap_or(0) as i32,
        ),
        Some("port_opened") => match v.get("port").and_then(Value::as_u64) {
            Some(p) if p <= u16::MAX as u64 => Ctl::Port {
                port: p as u16,
                url: v
                    .get("address")
                    .or_else(|| v.get("url"))
                    .and_then(Value::as_str)
                    .filter(|s| s.starts_with("http"))
                    .map(str::to_string),
            },
            _ => Ctl::Other,
        },
        _ => Ctl::Other,
    }
}

fn encode_in(i: In, tty: bool) -> Option<Message> {
    match (i, tty) {
        (In::Data(d), true) => Some(Message::binary(d)),
        (In::Data(d), false) => {
            let mut f = Vec::with_capacity(d.len() + 1);
            f.push(stream::STDIN);
            f.extend_from_slice(&d);
            Some(Message::binary(f))
        }
        (In::Eof, true) => None,
        (In::Eof, false) => Some(Message::binary(vec![stream::STDIN_EOF])),
        (In::Resize { cols, rows }, _) => Some(Message::text(
            json!({"type": "resize", "cols": cols, "rows": rows}).to_string(),
        )),
        (In::Signal(s), _) => {
            let s = s.trim_start_matches("SIG").to_string();
            Some(Message::text(
                json!({"type": "signal", "signal": s}).to_string(),
            ))
        }
    }
}

enum Decoded {
    Out(Out),
    Closed(String),
    Nothing,
}

fn decode(m: Message, tty: bool) -> Decoded {
    match m {
        Message::Binary(b) if tty => Decoded::Out(Out::Stdout(b.to_vec())),
        Message::Binary(b) => match b.first() {
            Some(&stream::STDOUT) => Decoded::Out(Out::Stdout(b[1..].to_vec())),
            Some(&stream::STDERR) => Decoded::Out(Out::Stderr(b[1..].to_vec())),
            Some(&stream::EXIT) => Decoded::Out(Out::Exit(b.get(1).copied().unwrap_or(0) as i32)),
            _ => Decoded::Nothing,
        },
        Message::Text(t) => match parse_ctl(t.as_str()) {
            Ctl::Exit(c) => Decoded::Out(Out::Exit(c)),
            Ctl::Port { port, url } => Decoded::Out(Out::PortOpened { port, url }),
            Ctl::SessionInfo(_) | Ctl::Other => Decoded::Nothing,
        },
        Message::Close(f) => Decoded::Closed(match f {
            Some(f) if !f.reason.is_empty() => format!("Sprites closed the session: {}", f.reason),
            _ => "Sprites closed the session".into(),
        }),
        _ => Decoded::Nothing,
    }
}

/// Bridge the exec socket and the session channels until exit, loss or detach.
async fn pump(
    mut ws: Ws,
    tty: bool,
    pending: Option<Message>,
    mut input: mpsc::Receiver<In>,
    out: mpsc::Sender<Out>,
) {
    // Returns true when the session is over.
    async fn deliver(ws: &mut Ws, out: &mpsc::Sender<Out>, d: Decoded) -> bool {
        match d {
            Decoded::Nothing => false,
            Decoded::Closed(reason) => {
                let _ = out.send(Out::Lost(reason)).await;
                true
            }
            Decoded::Out(o) => {
                let exit = matches!(o, Out::Exit(_));
                if out.send(o).await.is_err() {
                    let _ = ws.close(None).await;
                    return true;
                }
                if exit {
                    let _ = ws.close(None).await;
                }
                exit
            }
        }
    }
    if let Some(m) = pending
        && deliver(&mut ws, &out, decode(m, tty)).await
    {
        return;
    }
    loop {
        tokio::select! {
            i = input.recv() => match i {
                // Input dropped: detach politely; a detachable session keeps running.
                None => {
                    let _ = ws.close(None).await;
                    return;
                }
                Some(i) => {
                    if let Some(m) = encode_in(i, tty)
                        && let Err(e) = ws.send(m).await
                    {
                        let _ = out.send(Out::Lost(format!("connection lost: {e}"))).await;
                        return;
                    }
                }
            },
            m = ws.next() => match m {
                None => {
                    let _ = out.send(Out::Lost("connection closed".into())).await;
                    return;
                }
                Some(Err(e)) => {
                    let _ = out.send(Out::Lost(format!("connection lost: {e}"))).await;
                    return;
                }
                Some(Ok(m)) => {
                    if deliver(&mut ws, &out, decode(m, tty)).await {
                        return;
                    }
                }
            },
            _ = out.closed() => {
                let _ = ws.close(None).await;
                return;
            }
        }
    }
}

/// Query of an exec socket.
fn exec_query(req: &ExecReq) -> Vec<(&'static str, String)> {
    let mut q: Vec<(&'static str, String)> = req.argv.iter().map(|a| ("cmd", a.clone())).collect();
    q.push(("path", req.argv[0].clone()));
    q.push(("stdin", "true".into()));
    q.push(("tty", req.tty.to_string()));
    if req.tty {
        q.push(("cols", req.cols.max(1).to_string()));
        q.push(("rows", req.rows.max(1).to_string()));
    }
    if let Some(d) = req.cwd.as_deref().filter(|d| !d.is_empty()) {
        q.push(("dir", d.to_string()));
    }
    for (k, v) in &req.env {
        q.push(("env", format!("{k}={v}")));
    }
    if req.detachable {
        q.push(("detachable", "true".into()));
    }
    q
}

/// Run `fly` (or `flyctl`) with a 10 s limit; `None` when it is missing or fails.
async fn run_fly(args: &[&str]) -> Option<String> {
    for bin in ["fly", "flyctl"] {
        let child = tokio::process::Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let Ok(child) = child else { continue };
        let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await;
        return match out {
            Ok(Ok(o)) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).into()),
            _ => None,
        };
    }
    None
}

/// The org to mint a token in: the personal org, else the only (or first) one. Accepts both
/// `fly orgs list --json` shapes: `{slug: name}` and `[{slug, name, type}]`.
fn pick_fly_org(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    let mut slugs: Vec<(String, bool)> = Vec::new();
    match &v {
        Value::Object(o) => {
            for (slug, val) in o {
                let personal = slug == "personal"
                    || val
                        .as_str()
                        .is_some_and(|n| n.to_ascii_lowercase().contains("personal"));
                slugs.push((slug.clone(), personal));
            }
        }
        Value::Array(a) => {
            for o in a {
                let Some(slug) = o
                    .get("slug")
                    .or_else(|| o.get("Slug"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let ty = o
                    .get("type")
                    .or_else(|| o.get("Type"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                slugs.push((
                    slug.to_string(),
                    slug == "personal" || ty.eq_ignore_ascii_case("personal"),
                ));
            }
        }
        _ => return None,
    }
    slugs
        .iter()
        .find(|(_, p)| *p)
        .or_else(|| slugs.first())
        .map(|(s, _)| s.clone())
}

/// A token in the sprite CLI's config, if it keeps one in the file (it may use the keyring).
fn sprite_cli_token(text: &str) -> Option<String> {
    fn looks_like(s: &str) -> bool {
        let s = s.trim();
        s.split('/').count() >= 3 && s.split('/').all(|p| !p.is_empty()) && !s.contains(' ')
    }
    fn walk(v: &Value, key_hint: bool) -> Option<String> {
        match v {
            Value::String(s) if key_hint && looks_like(s) => Some(s.trim().to_string()),
            Value::Object(o) => {
                // Token-named keys first, then everything else.
                let tokenish = |k: &str| k.to_ascii_lowercase().contains("token");
                o.iter()
                    .filter(|(k, _)| tokenish(k))
                    .find_map(|(_, v)| walk(v, true))
                    .or_else(|| {
                        o.iter()
                            .filter(|(k, _)| !tokenish(k))
                            .find_map(|(_, v)| walk(v, false))
                    })
            }
            Value::Array(a) => a.iter().find_map(|v| walk(v, key_hint)),
            _ => None,
        }
    }
    walk(&serde_json::from_str(text).ok()?, false)
}

fn sessions_from(v: &Value) -> Vec<SessionInfo> {
    let list = match v {
        Value::Array(a) => a.as_slice(),
        Value::Object(o) => o
            .get("sessions")
            .or_else(|| o.get("data"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default(),
        _ => &[],
    };
    list.iter()
        .filter_map(|s| {
            let id = id_str(s.get("id")).or_else(|| id_str(s.get("session_id")))?;
            let command = match s.get("command") {
                Some(Value::String(c)) => c.clone(),
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => String::new(),
            };
            Some(SessionInfo {
                id,
                command,
                tty: s.get("tty").and_then(Value::as_bool).unwrap_or(false),
                active: s
                    .get("is_active")
                    .or_else(|| s.get("active"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                last_activity_at: ts(s.get("last_activity")).max(ts(s.get("created"))),
            })
        })
        .collect()
}

/// The checkpoint id in an NDJSON progress stream, if any line names one.
fn checkpoint_id(body: &str) -> Result<String> {
    let mut id = None;
    for line in body.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) == Some("error") {
            let m = v
                .get("error")
                .or_else(|| v.get("message"))
                .or_else(|| v.get("data"))
                .and_then(Value::as_str)
                .unwrap_or("checkpoint failed");
            return Err(CloudError::unavailable(m.to_string()));
        }
        for k in ["checkpoint_id", "id", "version"] {
            if let Some(s) = id_str(v.get(k)) {
                id = Some(s);
            }
        }
        if let Some(s) = v.get("checkpoint").and_then(|c| id_str(c.get("id"))) {
            id = Some(s);
        }
    }
    Ok(id.unwrap_or_else(|| "latest".into()))
}

impl Provider for Sprites {
    fn id(&self) -> &'static str {
        "sprites"
    }
    fn label(&self) -> &'static str {
        "Fly.io Sprites"
    }
    fn caps(&self) -> Caps {
        Caps {
            resize: true,
            reattach: true,
            checkpoints: true,
            port_urls: true,
            ..Caps::default()
        }
    }
    fn auth_methods(&self) -> Vec<AuthMethod> {
        vec![
            AuthMethod::PasteToken {
                label: "Sprites token".into(),
                help_url: "https://sprites.dev/account".into(),
                hint: "Looks like <org>/<id>/<secret>".into(),
            },
            AuthMethod::Import {
                source: "fly".into(),
                label: "Use my fly.io login".into(),
            },
            AuthMethod::Import {
                source: "sprite-cli".into(),
                label: "Use the sprite CLI login".into(),
            },
            AuthMethod::Env {
                var: "SPRITES_TOKEN".into(),
            },
        ]
    }

    fn verify<'a>(&'a self, cred: &'a Secret) -> BoxFut<'a, Result<Account>> {
        Box::pin(async move {
            if cred.is_empty() {
                return Err(CloudError::needs_auth("sprites"));
            }
            let url = self.url(&["sprites"], &[("max_results", "1".into())])?;
            let body = send(self.req(cred, reqwest::Method::GET, url)).await?;
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let org = v.get("org");
            let mut details = std::collections::BTreeMap::new();
            for k in ["running_limit", "warm_limit"] {
                if let Some(n) = org.and_then(|o| o.get(k)).filter(|n| !n.is_null()) {
                    details.insert(k.to_string(), n.to_string());
                }
            }
            Ok(Account {
                label: org
                    .and_then(|o| o.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("Sprites")
                    .to_string(),
                details,
            })
        })
    }

    fn import<'a>(&'a self, source: &'a str) -> BoxFut<'a, Result<Option<Secret>>> {
        Box::pin(async move {
            match source {
                "fly" => self.import_fly().await,
                "sprite-cli" => {
                    let path = crate::config::expand_home("~/.sprites/sprites.json");
                    let Ok(text) = tokio::fs::read_to_string(&path).await else {
                        return Ok(None);
                    };
                    Ok(sprite_cli_token(&text).map(Secret::new))
                }
                _ => Err(CloudError::new(
                    ErrorKind::InvalidParams,
                    format!("unknown import source {source:?} for sprites"),
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
            let auth = match self.cfg.url_auth.as_deref() {
                Some("public") => "public",
                _ => "sprite",
            };
            let body = json!({"name": spec.name, "url_settings": {"auth": auth}});
            let resp = send(
                self.req(cred, reqwest::Method::POST, self.url(&["sprites"], &[])?)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body.to_string()),
            )
            .await?;
            let parsed = serde_json::from_slice::<Value>(&resp)
                .ok()
                .and_then(|v| parse_box(&v));
            let mut b = match parsed {
                Some(b) => b,
                None => self.get_box(cred, &spec.name).await?,
            };
            if b.tags.is_none() {
                b.tags = Some(spec.tags.clone());
            }
            if b.state == BoxState::Unknown {
                b.state = BoxState::Creating;
            }
            Ok(b)
        })
    }

    fn get<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<RemoteBox>> {
        Box::pin(self.get_box(cred, id))
    }

    fn list<'a>(&'a self, cred: &'a Secret) -> BoxFut<'a, Result<Vec<RemoteBox>>> {
        Box::pin(async move {
            let mut out = Vec::new();
            let mut token: Option<String> = None;
            for _ in 0..100 {
                let mut q = vec![
                    ("prefix", naming::PREFIX.to_string()),
                    ("max_results", "100".to_string()),
                ];
                if let Some(t) = &token {
                    q.push(("continuation_token", t.clone()));
                }
                let url = self.url(&["sprites"], &q)?;
                let body = send(self.req(cred, reqwest::Method::GET, url)).await?;
                let v: Value = serde_json::from_slice(&body).map_err(|e| {
                    CloudError::unavailable(format!("unexpected Sprites response: {e}"))
                })?;
                let items = v
                    .get("sprites")
                    .or_else(|| v.get("data"))
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                out.extend(
                    items
                        .iter()
                        .filter_map(parse_box)
                        .filter(|b| b.tags.is_some()),
                );
                let more = v.get("has_more").and_then(Value::as_bool).unwrap_or(false);
                token = v
                    .get("next_continuation_token")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(str::to_string);
                if !more || token.is_none() {
                    break;
                }
            }
            Ok(out)
        })
    }

    fn destroy<'a>(&'a self, cred: &'a Secret, id: &'a str) -> BoxFut<'a, Result<()>> {
        Box::pin(async move {
            let url = self.url(&["sprites", id], &[])?;
            match send(self.req(cred, reqwest::Method::DELETE, url)).await {
                Ok(_) => Ok(()),
                Err(e) if e.kind == ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        })
    }

    fn suspend<'a>(&'a self, _cred: &'a Secret, _id: &'a str) -> BoxFut<'a, Result<()>> {
        // Sprites sleep by themselves when idle.
        Box::pin(async { Ok(()) })
    }

    fn resume<'a>(&'a self, _cred: &'a Secret, _id: &'a str) -> BoxFut<'a, Result<()>> {
        // Any request wakes a Sprite.
        Box::pin(async { Ok(()) })
    }

    fn checkpoint<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
        note: &'a str,
    ) -> BoxFut<'a, Result<String>> {
        Box::pin(async move {
            let url = self.url(&["sprites", id, "checkpoint"], &[])?;
            let mut rb = self
                .http
                .post(url)
                .bearer_auth(cred.expose())
                .timeout(Duration::from_secs(600));
            if !note.is_empty() {
                rb = rb
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(json!({"comment": note}).to_string());
            }
            let mut resp = rb.send().await.map_err(transport)?;
            let status = resp.status().as_u16();
            let mut body = Vec::new();
            while let Some(c) = resp.chunk().await.map_err(transport)? {
                body.extend_from_slice(&c);
                if body.len() > 4 * 1024 * 1024 {
                    break;
                }
            }
            if !(200..300).contains(&status) {
                return Err(http_error(status, &body));
            }
            checkpoint_id(&String::from_utf8_lossy(&body))
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
            let url = self.ws_url(&["sprites", id, "exec"], &exec_query(&req))?;
            // Only sessions that can be attached again need their id before going on.
            let wait = req.tty || req.detachable;
            self.start(cred, url, req.tty, wait).await
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
            let url = self.ws_url(&["sprites", id, "exec", session], &[])?;
            let mut ws = self.open_ws(cred, url).await?;
            // The first message says whether the session has a terminal.
            let mut tty = true;
            let mut sid = session.to_string();
            let mut pending = None;
            match tokio::time::timeout(INFO_WAIT, ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    let v: Value = serde_json::from_str(t.as_str()).unwrap_or(Value::Null);
                    if v.get("type").and_then(Value::as_str) == Some("session_info") {
                        tty = v.get("tty").and_then(Value::as_bool).unwrap_or(true);
                        if let Some(i) = id_str(v.get("session_id")).or_else(|| id_str(v.get("id")))
                        {
                            sid = i;
                        }
                    } else {
                        pending = Some(Message::Text(t));
                    }
                }
                Ok(Some(Ok(m))) => pending = Some(m),
                Ok(Some(Err(e))) => {
                    return Err(CloudError::unavailable(format!(
                        "attaching to the Sprites session failed: {e}"
                    )));
                }
                Ok(None) => return Err(CloudError::not_found("the session has ended")),
                Err(_) => {}
            }
            if tty
                && cols > 0
                && rows > 0
                && let Some(m) = encode_in(In::Resize { cols, rows }, true)
            {
                let _ = ws.send(m).await;
            }
            let (in_tx, in_rx) = mpsc::channel(64);
            let (out_tx, out_rx) = mpsc::channel(256);
            tokio::spawn(pump(ws, tty, pending, in_rx, out_tx));
            Ok(Session {
                id: sid,
                tty,
                input: in_tx,
                output: out_rx,
            })
        })
    }

    fn sessions<'a>(
        &'a self,
        cred: &'a Secret,
        id: &'a str,
    ) -> BoxFut<'a, Result<Vec<SessionInfo>>> {
        Box::pin(async move {
            let url = self.url(&["sprites", id, "exec"], &[])?;
            let body = send(self.req(cred, reqwest::Method::GET, url)).await?;
            let v: Value = serde_json::from_slice(&body).map_err(|e| {
                CloudError::unavailable(format!("unexpected Sprites response: {e}"))
            })?;
            Ok(sessions_from(&v))
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
            let url = self.url(
                &["sprites", id, "fs", "write"],
                &[
                    ("path", path.to_string()),
                    ("mode", format!("{:04o}", mode & 0o7777)),
                    // The parameter names follow the official Go SDK
                    // (superfly/sprites-go, filesystem.go WriteFileContext).
                    ("mkdirParents", "true".into()),
                ],
            )?;
            send(
                self.req(cred, reqwest::Method::PUT, url)
                    .timeout(Duration::from_secs(600))
                    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                    .body(data),
            )
            .await?;
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
            // A Sprite's URL routes to its default HTTP port only.
            if port != HTTP_PORT {
                return Ok(None);
            }
            Ok(self.get_box(cred, id).await?.url)
        })
    }
}

#[cfg(test)]
#[path = "sprites_tests.rs"]
mod tests;
