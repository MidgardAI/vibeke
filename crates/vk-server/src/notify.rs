//! Notification pipeline and native OS notifier (08 §7).
//!
//! `notification.created → rules (notifications.on) → presence → quiet hours → coalescing →
//! channels`. Toasts and host-terminal OSC are drawn/forwarded by clients; this module records
//! the decision on the notification (`channels`) and delivers the **native** channel itself:
//!
//! * macOS: `terminal-notifier` when installed (click runs `vibeke focus <url>`), else
//!   `osascript -e 'display notification …'` (no click action: a script notification can't carry
//!   one; the signed helper bundle with `UNUserNotificationCenter` is not built yet).
//! * Linux: `notify-send` (org.freedesktop.Notifications over D-Bus) with a default action and
//!   `--wait`; clicking runs `vibeke focus <url>`. Older `notify-send` without `--action` falls
//!   back to a plain notification.
//! * `VIBEKE_NOTIFIER=none | auto | log:<file>` overrides the backend (`log:` appends JSON lines
//!   — the fake used by end-to-end tests). Tests inject a notifier with [`set_notifier`].
//!
//! Native delivery is only automatic once an attached client reported its host terminal
//! (`render.attach`/`client.hello` param `host`): headless servers (CI, scripts, tests) never pop
//! OS notifications. `VIBEKE_NOTIFIER=auto` forces it.
//!
//! Click-to-focus: `client.focus {pane | url}` focuses the pane in the most recently active
//! attached client and raises its host terminal (macOS: `tell application id "<bundle>" to
//! activate`, bundle id from the client's `host` metadata, else the server's own
//! `__CFBundleIdentifier`/`TERM_PROGRAM`). Linux raising (xdg-activation) is not implemented.

use crate::Server;
use crate::api::{Ctx, R, b, err, invalid, not_found, resolve_pane, s};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vk_proto::model::Notification;
use vk_proto::rpc::ErrorKind;

/// Host terminal of an attached client (captured at attach).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HostInfo {
    /// macOS app bundle id (`__CFBundleIdentifier`).
    pub bundle_id: Option<String>,
    pub term_program: Option<String>,
}

impl HostInfo {
    pub fn from_json(v: &Value) -> HostInfo {
        let g = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        HostInfo {
            bundle_id: g("bundle_id").or_else(|| g("__CFBundleIdentifier")),
            term_program: g("term_program").or_else(|| g("TERM_PROGRAM")),
        }
    }
    pub fn from_env<'a>(env: impl IntoIterator<Item = &'a (String, String)>) -> HostInfo {
        let mut h = HostInfo::default();
        for (k, v) in env {
            match k.as_str() {
                "__CFBundleIdentifier" if !v.is_empty() => h.bundle_id = Some(v.clone()),
                "TERM_PROGRAM" if !v.is_empty() && v != "vibeke" => {
                    h.term_program = Some(v.clone())
                }
                _ => {}
            }
        }
        h
    }
    pub fn is_empty(&self) -> bool {
        self.bundle_id.is_none() && self.term_program.is_none()
    }
    /// Bundle id to activate: explicit, else mapped from `TERM_PROGRAM`.
    pub fn bundle(&self) -> Option<String> {
        if let Some(b) = &self.bundle_id {
            return Some(b.clone());
        }
        let tp = self.term_program.as_deref()?;
        Some(
            match tp {
                "iTerm.app" => "com.googlecode.iterm2",
                "Apple_Terminal" => "com.apple.Terminal",
                "ghostty" => "com.mitchellh.ghostty",
                "WezTerm" => "com.github.wez.wezterm",
                "kitty" | "xterm-kitty" => "net.kovidgoyal.kitty",
                "vscode" => "com.microsoft.VSCode",
                "WarpTerminal" => "dev.warp.Warp-Stable",
                "Hyper" => "co.zeit.hyper",
                "Tabby" => "org.tabby",
                "alacritty" | "Alacritty" => "org.alacritty",
                _ => return None,
            }
            .to_string(),
        )
    }
}

/// One native notification.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Note {
    pub title: String,
    pub body: String,
    pub urgency: String,
    /// `vibeke://focus?session=…&pane=…`
    pub url: Option<String>,
    /// Command run when the notification is clicked (`vibeke --session s focus <url>`).
    pub on_click: Vec<String>,
    /// Coalescing group (one per agent/pane).
    pub group: String,
}

pub trait Notifier: Send + Sync {
    fn name(&self) -> String;
    fn notify(&self, n: &Note) -> Result<()>;
    /// Raise the host terminal window; `Ok(false)` when unsupported.
    fn raise(&self, host: &HostInfo) -> Result<bool>;
}

pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

/// AppleScript string literal.
pub fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// POSIX shell single-quoting.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn raise_macos(host: &HostInfo) -> Result<bool> {
    let Some(bundle) = host.bundle() else {
        return Ok(false);
    };
    let script = format!(
        "tell application id {} to activate",
        applescript_str(&bundle)
    );
    let st = std::process::Command::new("/usr/bin/osascript")
        .args(["-e", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("osascript")?;
    Ok(st.success())
}

/// macOS: `terminal-notifier` (click action) or `osascript` (none).
pub struct MacNotifier {
    pub terminal_notifier: Option<PathBuf>,
}

impl MacNotifier {
    /// argv for one notification (pure; tested).
    pub fn argv(&self, n: &Note) -> Vec<String> {
        match &self.terminal_notifier {
            Some(tn) => {
                let mut v = vec![
                    tn.to_string_lossy().into_owned(),
                    "-title".into(),
                    "Vibeke".into(),
                    "-subtitle".into(),
                    n.title.clone(),
                    "-message".into(),
                    if n.body.is_empty() {
                        " ".into()
                    } else {
                        n.body.clone()
                    },
                    "-group".into(),
                    n.group.clone(),
                ];
                if !n.on_click.is_empty() {
                    v.push("-execute".into());
                    v.push(
                        n.on_click
                            .iter()
                            .map(|a| sh_quote(a))
                            .collect::<Vec<_>>()
                            .join(" "),
                    );
                }
                v
            }
            None => {
                let script = format!(
                    "display notification {} with title \"Vibeke\" subtitle {}",
                    applescript_str(&n.body),
                    applescript_str(&n.title)
                );
                vec!["/usr/bin/osascript".into(), "-e".into(), script]
            }
        }
    }
}

impl Notifier for MacNotifier {
    fn name(&self) -> String {
        if self.terminal_notifier.is_some() {
            "terminal-notifier".into()
        } else {
            "osascript".into()
        }
    }
    fn notify(&self, n: &Note) -> Result<()> {
        let argv = self.argv(n);
        std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("spawn notifier")?;
        Ok(())
    }
    fn raise(&self, host: &HostInfo) -> Result<bool> {
        raise_macos(host)
    }
}

/// Linux: `notify-send` (freedesktop notifications over D-Bus).
pub struct LinuxNotifier {
    pub notify_send: PathBuf,
}

impl LinuxNotifier {
    pub fn argv(&self, n: &Note, with_action: bool) -> Vec<String> {
        let urgency = match n.urgency.as_str() {
            "high" | "critical" => "critical",
            "low" => "low",
            _ => "normal",
        };
        let mut v = vec![
            self.notify_send.to_string_lossy().into_owned(),
            "--app-name=Vibeke".into(),
            format!("--urgency={urgency}"),
            format!("--hint=string:x-canonical-private-synchronous:{}", n.group),
        ];
        if with_action && !n.on_click.is_empty() {
            v.push("--action=default=Focus".into());
            v.push("--wait".into());
        }
        v.push(n.title.clone());
        if !n.body.is_empty() {
            v.push(n.body.clone());
        }
        v
    }
}

impl Notifier for LinuxNotifier {
    fn name(&self) -> String {
        "notify-send".into()
    }
    fn notify(&self, n: &Note) -> Result<()> {
        let with = self.argv(n, true);
        let plain = self.argv(n, false);
        let click = n.on_click.clone();
        // `--wait` blocks until the notification closes: run it off the server's threads.
        std::thread::spawn(move || {
            let out = std::process::Command::new(&with[0])
                .args(&with[1..])
                .stderr(std::process::Stdio::null())
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    if String::from_utf8_lossy(&o.stdout).trim() == "default" && !click.is_empty() {
                        let _ = std::process::Command::new(&click[0])
                            .args(&click[1..])
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status();
                    }
                }
                _ => {
                    // notify-send < 0.7.9 has no --action/--wait.
                    let _ = std::process::Command::new(&plain[0])
                        .args(&plain[1..])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
        });
        Ok(())
    }
    fn raise(&self, _host: &HostInfo) -> Result<bool> {
        Ok(false)
    }
}

/// Appends JSON lines (`{"event":"notify",…}` / `{"event":"raise",…}`): the e2e fake.
pub struct LogNotifier {
    pub path: PathBuf,
}

impl LogNotifier {
    fn append(&self, v: Value) -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        // One write per line: O_APPEND keeps concurrent writers from interleaving.
        f.write_all(format!("{v}\n").as_bytes())?;
        Ok(())
    }
}

impl Notifier for LogNotifier {
    fn name(&self) -> String {
        "log".into()
    }
    fn notify(&self, n: &Note) -> Result<()> {
        self.append(json!({"event": "notify", "note": n}))
    }
    fn raise(&self, host: &HostInfo) -> Result<bool> {
        self.append(json!({"event": "raise", "host": host, "bundle": host.bundle()}))?;
        Ok(true)
    }
}

/// Records calls in memory (unit tests).
#[derive(Default)]
pub struct MemNotifier {
    pub notes: Mutex<Vec<Note>>,
    pub raised: Mutex<Vec<HostInfo>>,
}

impl Notifier for MemNotifier {
    fn name(&self) -> String {
        "memory".into()
    }
    fn notify(&self, n: &Note) -> Result<()> {
        self.notes.lock().unwrap().push(n.clone());
        Ok(())
    }
    fn raise(&self, host: &HostInfo) -> Result<bool> {
        self.raised.lock().unwrap().push(host.clone());
        Ok(true)
    }
}

/// The platform backend, if any is installed.
pub fn platform_notifier() -> Option<Arc<dyn Notifier>> {
    if cfg!(target_os = "macos") {
        let osa = Path::new("/usr/bin/osascript").is_file();
        let tn = which("terminal-notifier");
        if tn.is_none() && !osa {
            return None;
        }
        return Some(Arc::new(MacNotifier {
            terminal_notifier: tn,
        }));
    }
    which("notify-send").map(|p| Arc::new(LinuxNotifier { notify_send: p }) as Arc<dyn Notifier>)
}

#[derive(Default)]
pub struct State {
    /// Injected backend (tests); wins over env and detection.
    injected: Mutex<Option<Arc<dyn Notifier>>>,
    /// client id → host terminal metadata.
    hosts: Mutex<HashMap<String, HostInfo>>,
    /// Coalescing: group → (last native delivery, merged count since).
    recent: Mutex<HashMap<String, (Instant, u32)>>,
}

pub fn set_notifier(server: &Server, n: Option<Arc<dyn Notifier>>) {
    *server.notifier.injected.lock().unwrap() = n;
}

/// Record a client's host terminal (`host` param of `render.attach` / `client.hello`).
pub fn record_host(server: &Server, client: &str, host: Option<&Value>) {
    let Some(h) = host.map(HostInfo::from_json).filter(|h| !h.is_empty()) else {
        return;
    };
    server
        .notifier
        .hosts
        .lock()
        .unwrap()
        .insert(client.to_string(), h);
}

/// Host terminals recorded per client.
pub fn hosts(server: &Server) -> HashMap<String, HostInfo> {
    server.notifier.hosts.lock().unwrap().clone()
}

/// Which backend applies now: `(notifier, reason when none)`.
fn backend(server: &Server) -> (Option<Arc<dyn Notifier>>, &'static str) {
    if let Some(n) = server.notifier.injected.lock().unwrap().clone() {
        return (Some(n), "");
    }
    match std::env::var("VIBEKE_NOTIFIER").ok().as_deref() {
        Some("none" | "off") => return (None, "disabled"),
        Some(v) if v.starts_with("log:") => {
            return (
                Some(Arc::new(LogNotifier {
                    path: PathBuf::from(&v[4..]),
                })),
                "",
            );
        }
        Some("auto") => {
            return match platform_notifier() {
                Some(n) => (Some(n), ""),
                None => (None, "unavailable"),
            };
        }
        _ => {}
    }
    if server.notifier.hosts.lock().unwrap().is_empty() {
        // Headless (no client reported a host terminal): never pop OS notifications.
        return (None, "headless");
    }
    match platform_notifier() {
        Some(n) => (Some(n), ""),
        None => (None, "unavailable"),
    }
}

/// `notifications.channels`, or derived from `notifications.channel`.
pub fn channels(cfg: &vk_config::Notifications) -> Vec<String> {
    if !cfg.channels.is_empty() {
        return cfg.channels.clone();
    }
    let v: &[&str] = match cfg.channel {
        vk_config::NotifyChannel::Native => &["toast", "native"],
        vk_config::NotifyChannel::Osc => &["toast", "osc"],
        vk_config::NotifyChannel::Both => &["toast", "native", "osc"],
        vk_config::NotifyChannel::None => &["toast"],
    };
    v.iter().map(|s| s.to_string()).collect()
}

/// `notifications.on` gate for a notification kind.
pub fn kind_enabled(on: &vk_config::NotifyOn, kind: &str, title: &str) -> bool {
    match kind {
        "interaction" => {
            if title.contains("asks") || title.contains("question") {
                on.needs_answer
            } else {
                on.needs_approval
            }
        }
        "agent_state" => on.done,
        "agent_error" => on.error,
        "bell" => on.bell,
        "osc9" | "osc99" | "osc777" => on.osc,
        "remote_disconnected" => on.remote_disconnected,
        _ => true,
    }
}

/// Minutes since local midnight.
fn local_minutes(now_secs: i64) -> u32 {
    // SAFETY: localtime_r writes into the provided struct only.
    unsafe {
        let t: libc::time_t = now_secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        (tm.tm_hour * 60 + tm.tm_min) as u32
    }
}

/// `"22:00-07:00"` contains `minute` (wraps midnight). Empty or malformed → false.
pub fn in_quiet_hours(spec: &str, minute: u32) -> bool {
    let parse = |t: &str| -> Option<u32> {
        let (h, m) = t.trim().split_once(':')?;
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        (h < 24 && m < 60).then_some(h * 60 + m)
    };
    let Some((a, b)) = spec.split_once('-') else {
        return false;
    };
    let (Some(a), Some(b)) = (parse(a), parse(b)) else {
        return false;
    };
    if a <= b {
        minute >= a && minute < b
    } else {
        minute >= a || minute < b
    }
}

pub fn focus_url(session: &str, pane: &str) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|c| match c {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (c as char).to_string()
                }
                _ => format!("%{c:02X}"),
            })
            .collect()
    };
    format!("vibeke://focus?session={}&pane={}", enc(session), enc(pane))
}

/// `(session, pane)` from `vibeke://focus?session=…&pane=…`.
pub fn parse_focus_url(url: &str) -> Option<(Option<String>, String)> {
    let q = url.strip_prefix("vibeke://focus")?.strip_prefix('?')?;
    let dec = |s: &str| -> String {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%'
                && i + 2 < b.len()
                && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
            out.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    let (mut session, mut pane) = (None, None);
    for kv in q.split('&') {
        match kv.split_once('=') {
            Some(("session", v)) => session = Some(dec(v)),
            Some(("pane", v)) => pane = Some(dec(v)),
            _ => {}
        }
    }
    Some((session, pane?))
}

/// A pane is "in front of the user": focused and visible in a client whose host window has focus.
fn presence(server: &Server, pane: &str) -> bool {
    server.clients.lock().unwrap().values().any(|st| {
        st.kind == "tui"
            && st.host_focused
            && st.focus.pane.as_deref() == Some(pane)
            && st.visible.iter().any(|v| v == pane)
    })
}

/// Run the pipeline for a freshly created notification; returns it with `channels` filled.
/// Must be called without `core` held (reads `core`, locks `clients`).
pub fn deliver(server: &Server, mut n: Notification) -> Notification {
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c.notifications)
        .unwrap_or_default();
    let wanted = channels(&cfg);
    let mut out: Vec<String> = Vec::new();
    let mut external = true;
    if !kind_enabled(&cfg.on, &n.kind, &n.title) {
        out.push(format!("filtered:{}", n.kind));
        external = false;
    }
    if external
        && cfg.suppress_when_focused
        && n.pane.as_deref().is_some_and(|p| presence(server, p))
    {
        out.push("suppressed:focused".into());
        external = false;
    }
    if external
        && n.urgency != "high"
        && in_quiet_hours(&cfg.quiet_hours, local_minutes(n.created_at_ms / 1000))
    {
        out.push("quiet_hours".into());
        external = false;
    }
    let group = n.pane.clone().unwrap_or_else(|| format!("kind:{}", n.kind));
    if external && wanted.iter().any(|c| c == "native") {
        let window = Duration::from_millis(cfg.coalesce_ms as u64);
        let mut recent = server.notifier.recent.lock().unwrap();
        recent.retain(|_, (at, _)| at.elapsed() < window.max(Duration::from_secs(1)) * 4);
        match recent.get_mut(&group) {
            Some((at, count)) if at.elapsed() < window => {
                *count += 1;
                out.push("coalesced".into());
                external = false;
            }
            _ => {
                recent.insert(group.clone(), (Instant::now(), 0));
            }
        }
    }
    for c in &wanted {
        match c.as_str() {
            "toast" => out.push("toast".into()),
            "native" if external => {
                let (backend, why) = backend(server);
                match backend {
                    Some(b) => {
                        let pane_ref = n
                            .pane
                            .as_deref()
                            .and_then(|p| server.with_core(|c| c.pane(p).map(|x| x.id.clone())));
                        let url = pane_ref
                            .as_deref()
                            .map(|p| focus_url(&server.opts.session, p));
                        let on_click = url
                            .as_ref()
                            .map(|u| {
                                vec![
                                    server.opts.bin.to_string_lossy().into_owned(),
                                    "--session".into(),
                                    server.opts.session.clone(),
                                    "focus".into(),
                                    u.clone(),
                                ]
                            })
                            .unwrap_or_default();
                        let note = Note {
                            title: n.title.clone(),
                            body: n.body.clone(),
                            urgency: n.urgency.clone(),
                            url,
                            on_click,
                            group: format!("vibeke-{}-{group}", server.opts.session),
                        };
                        match b.notify(&note) {
                            Ok(()) => out.push("native".into()),
                            Err(e) => {
                                tracing::debug!(error = %e, "native notification failed");
                                out.push("native:failed".into());
                            }
                        }
                    }
                    None => {
                        out.push(format!("native:{why}"));
                        // `channel = native` falls back to the host terminal (OSC) when native
                        // isn't available (08 §7.1).
                        if cfg.channels.is_empty() && !wanted.iter().any(|w| w == "osc") {
                            out.push("osc".into());
                        }
                    }
                }
            }
            "osc" | "sound" | "bell" if external => out.push(c.clone()),
            _ => {}
        }
    }
    n.channels = out;
    let id = n.id.clone();
    let ch = n.channels.clone();
    server.with_core(|c| {
        if let Some(x) = c.notifications.iter_mut().find(|x| x.id == id) {
            x.channels = ch;
        }
    });
    n
}

/// Most recently active TUI client.
pub fn recent_client(server: &Server) -> Option<String> {
    server
        .clients
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, s)| s.kind == "tui")
        .max_by_key(|(_, s)| s.last_active)
        .map(|(k, _)| k.clone())
}

pub const METHODS: &[(&str, bool)] = &[("client.focus", true), ("notification.config", false)];

pub fn api(server: &Server, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        // `{pane | url, raise?: true}`: focus in the most recently active client + raise host.
        "client.focus" => (|| {
            if ctx.pane_scope.is_some() {
                return Err(err(
                    ErrorKind::PermissionDenied,
                    "client.focus is not allowed from a pane (agents can't move the user's focus)",
                ));
            }
            let target = match (s(p, "url"), s(p, "pane")) {
                (Some(u), _) => {
                    let (sess, pane) = parse_focus_url(u)
                        .ok_or_else(|| invalid(format!("not a vibeke://focus URL: {u}")))?;
                    if let Some(sess) = sess
                        && sess != server.opts.session
                    {
                        return Err(invalid(format!(
                            "URL is for session {sess}, this is {}",
                            server.opts.session
                        ))
                        .details(json!({"session": sess})));
                    }
                    pane
                }
                (None, Some(pn)) => pn.to_string(),
                _ => return Err(invalid("pane or url required")),
            };
            let pane =
                resolve_pane(server, ctx, Some(&target)).map_err(|_| not_found("pane", &target))?;
            let client = recent_client(server);
            if let Some(c) = &client {
                server.focus_pane(c, &pane.id);
            }
            let mut raised = false;
            let mut host = None;
            if b(p, "raise").unwrap_or(true) {
                let h = client
                    .as_ref()
                    .and_then(|c| server.notifier.hosts.lock().unwrap().get(c).cloned())
                    .filter(|h| !h.is_empty())
                    .unwrap_or_else(|| HostInfo::from_env(&server.opts.env));
                let (backend, _) = backend(server);
                let backend = backend.or_else(platform_notifier);
                if let Some(bk) = backend {
                    raised = bk.raise(&h).unwrap_or(false);
                }
                host = Some(h);
            }
            Ok(
                json!({"pane": pane, "client": client, "focused": client.is_some(), "raised": raised, "host": host}),
            )
        })(),
        "notification.config" => {
            let cfg = vk_config::Config::load(vk_config::config_path())
                .map(|(c, _)| c.notifications)
                .unwrap_or_default();
            let (backend, why) = backend(server);
            Ok(json!({
                "channels": channels(&cfg),
                "native": {"backend": backend.map(|b| b.name()), "unavailable_reason": if why.is_empty() { Value::Null } else { json!(why) }},
                "rules": {"on": cfg.on, "suppress_when_focused": cfg.suppress_when_focused,
                          "coalesce_ms": cfg.coalesce_ms, "quiet_hours": cfg.quiet_hours},
                "hosts": server.notifier.hosts.lock().unwrap().clone(),
            }))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_hours_wrap() {
        assert!(in_quiet_hours("22:00-07:00", 23 * 60));
        assert!(in_quiet_hours("22:00-07:00", 6 * 60 + 59));
        assert!(!in_quiet_hours("22:00-07:00", 7 * 60));
        assert!(in_quiet_hours("12:00-13:30", 13 * 60));
        assert!(!in_quiet_hours("", 0));
        assert!(!in_quiet_hours("bogus", 0));
    }

    #[test]
    fn focus_url_roundtrip() {
        let u = focus_url("my sess", "01ABC");
        assert_eq!(u, "vibeke://focus?session=my%20sess&pane=01ABC");
        assert_eq!(
            parse_focus_url(&u),
            Some((Some("my sess".into()), "01ABC".into()))
        );
        assert_eq!(
            parse_focus_url("vibeke://focus?pane=w1:p2").unwrap().1,
            "w1:p2"
        );
        assert!(parse_focus_url("https://x").is_none());
    }

    #[test]
    fn host_bundle_mapping() {
        let h = HostInfo::from_env(&[
            ("TERM_PROGRAM".to_string(), "ghostty".to_string()),
            ("HOME".to_string(), "/x".to_string()),
        ]);
        assert_eq!(h.bundle().as_deref(), Some("com.mitchellh.ghostty"));
        let h = HostInfo::from_json(&json!({"bundle_id": "com.example.term", "term_program": "x"}));
        assert_eq!(h.bundle().as_deref(), Some("com.example.term"));
        assert!(HostInfo::from_env(&[("TERM_PROGRAM".into(), "vibeke".into())]).is_empty());
    }

    #[test]
    fn backend_argv() {
        let note = Note {
            title: "claude needs \"approval\"".into(),
            body: "rm -rf build".into(),
            urgency: "high".into(),
            url: Some("vibeke://focus?pane=p".into()),
            on_click: vec![
                "/bin/vibeke".into(),
                "focus".into(),
                "vibeke://focus?pane=p".into(),
            ],
            group: "g".into(),
        };
        let osa = MacNotifier {
            terminal_notifier: None,
        }
        .argv(&note);
        assert_eq!(osa[0], "/usr/bin/osascript");
        assert!(
            osa[2].contains("subtitle \"claude needs \\\"approval\\\"\""),
            "{}",
            osa[2]
        );
        let tn = MacNotifier {
            terminal_notifier: Some("/opt/tn".into()),
        }
        .argv(&note);
        let i = tn.iter().position(|a| a == "-execute").unwrap();
        assert_eq!(tn[i + 1], "'/bin/vibeke' 'focus' 'vibeke://focus?pane=p'");
        let ln = LinuxNotifier {
            notify_send: "/usr/bin/notify-send".into(),
        };
        let a = ln.argv(&note, true);
        assert!(a.contains(&"--urgency=critical".to_string()));
        assert!(a.contains(&"--action=default=Focus".to_string()));
        assert!(!ln.argv(&note, false).contains(&"--wait".to_string()));
    }

    #[test]
    fn channel_derivation_and_rules() {
        let mut c = vk_config::Notifications::default();
        assert_eq!(channels(&c), vec!["toast", "native"]);
        c.channel = vk_config::NotifyChannel::Both;
        assert_eq!(channels(&c), vec!["toast", "native", "osc"]);
        c.channels = vec!["toast".into()];
        assert_eq!(channels(&c), vec!["toast"]);
        let mut on = vk_config::NotifyOn::default();
        assert!(!kind_enabled(&on, "bell", ""));
        assert!(kind_enabled(&on, "agent_state", "x is done"));
        on.done = false;
        assert!(!kind_enabled(&on, "agent_state", "x is done"));
        assert!(kind_enabled(&on, "plugin", ""));
    }
}
