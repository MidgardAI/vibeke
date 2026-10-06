//! Theme auto light/dark (08 §11 `[theme]`, 03 §10.4), client side: detect the host terminal's
//! appearance, report it with `client.appearance {dark, source}` to every connected machine,
//! and switch the chrome between `theme.dark_name` / `theme.light_name`.
//!
//! Detection: the startup probe (before the event reader starts) asks for the background colour
//! (OSC 11) and the colour-scheme report (`CSI ? 996 n` → `CSI ? 997 ; 1|2 n`, 1 = dark); the
//! 997 report wins when both arrive. Mode 2031 (unsolicited 997 reports on change) is *not*
//! enabled: crossterm's input parser doesn't know `CSI ? … n` and would swallow keystrokes
//! after one. Instead the host is re-queried after it regains focus (at most every 3 s, only
//! with `theme.mode = "auto"`) and from the palette (`theme_detect`), with the event reader
//! stopped so the replies are read raw.
//!
//! Panes' OSC 10/11 colour queries are answered by the server's VT engine (it owns the PTYs),
//! from the palette that follows `SessionModel.appearance`; the TUI never answers them, so
//! there are no double replies.

use crate::app::{App, Pending};
use crate::parity::Reply;
use crate::screen::Grid;
use crate::theme::Theme;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use vk_proto::model::Appearance;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Osc11,
    Csi996,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Osc11 => "osc11",
            Source::Csi996 => "csi996",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detected {
    pub dark: bool,
    pub source: Source,
}

#[derive(Default)]
pub struct State {
    /// The host appearance this client detected.
    pub detected: Option<Detected>,
    /// Machine → what we last reported there.
    pub reported: HashMap<usize, bool>,
    pub last_probe: Option<Instant>,
    pub want_probe: bool,
    /// Theme name currently applied to the chrome.
    pub applied: String,
}

const REPROBE_MIN: Duration = Duration::from_secs(3);

/// `CSI ? 997 ; 1 n` (dark) / `CSI ? 997 ; 2 n` (light), the reply to `CSI ? 996 n` and the
/// mode 2031 notification. The last report in `buf` wins.
pub fn parse_997(buf: &[u8]) -> Option<bool> {
    let mut found = None;
    let pat = b"\x1b[?997;";
    let mut i = 0;
    while i + pat.len() < buf.len() {
        if &buf[i..i + pat.len()] == pat {
            let rest = &buf[i + pat.len()..];
            let end = rest.iter().position(|b| !b.is_ascii_digit());
            if let Some(e) = end
                && rest.get(e) == Some(&b'n')
            {
                match &rest[..e] {
                    b"1" => found = Some(true),
                    b"2" => found = Some(false),
                    _ => {}
                }
            }
        }
        i += 1;
    }
    found
}

/// Parse `rgb:R/G/B` (1–4 hex digits per channel) or `#rrggbb` to 0..=1.
fn color(s: &str) -> Option<(f64, f64, f64)> {
    let chan = |h: &str| -> Option<f64> {
        if h.is_empty() || h.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        Some(v as f64 / ((1u32 << (4 * h.len())) - 1) as f64)
    };
    if let Some(rest) = s.strip_prefix("rgb:").or_else(|| s.strip_prefix("rgba:")) {
        let mut it = rest.split('/');
        return Some((chan(it.next()?)?, chan(it.next()?)?, chan(it.next()?)?));
    }
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    Some((chan(&h[0..2])?, chan(&h[2..4])?, chan(&h[4..6])?))
}

/// OSC 11 reply (`ESC ] 11 ; rgb:… BEL|ST`) → dark when the background's luminance is ≤ 0.5.
pub fn parse_osc11(buf: &[u8]) -> Option<bool> {
    let pat = b"\x1b]11;";
    let mut found = None;
    let mut i = 0;
    while i + pat.len() <= buf.len() {
        if &buf[i..i + pat.len()] == pat {
            let rest = &buf[i + pat.len()..];
            let end = rest
                .iter()
                .position(|&b| b == 0x07 || b == 0x1b)
                .unwrap_or(rest.len());
            if let Ok(s) = std::str::from_utf8(&rest[..end])
                && let Some((r, g, b)) = color(s.trim())
            {
                found = Some(0.2126 * r + 0.7152 * g + 0.0722 * b <= 0.5);
            }
            i += pat.len() + end;
            continue;
        }
        i += 1;
    }
    found
}

/// Everything the host answered: the colour-scheme report wins over the background colour.
pub fn parse_reports(buf: &[u8]) -> Option<Detected> {
    if let Some(dark) = parse_997(buf) {
        return Some(Detected {
            dark,
            source: Source::Csi996,
        });
    }
    parse_osc11(buf).map(|dark| Detected {
        dark,
        source: Source::Osc11,
    })
}

/// The queries: background colour, colour-scheme report, then DA1 as the sentinel.
pub const QUERIES: &[u8] = b"\x1b]11;?\x1b\\\x1b[?996n\x1b[c";

static STARTUP: Mutex<Option<Detected>> = Mutex::new(None);

/// Called by the startup probe with every byte the host answered.
pub fn record_probe(buf: &[u8]) {
    *STARTUP.lock().unwrap() = parse_reports(buf);
}

pub fn startup() -> Option<Detected> {
    *STARTUP.lock().unwrap()
}

/// Re-query the host with nobody else reading the terminal: write the queries and read raw
/// replies until the DA1 sentinel (≤ 150 ms). Bytes that aren't replies (keys typed in that
/// window) are dropped.
pub fn reprobe() -> Option<Detected> {
    use std::io::{Read, Write};
    // Let a reader that was just stopped finish its last read first.
    std::thread::sleep(Duration::from_millis(15));
    let mut out = std::io::stdout();
    if out.write_all(QUERIES).is_err() || out.flush().is_err() {
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(150);
    let mut buf = Vec::new();
    let mut stdin = std::io::stdin();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: polling stdin with a valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) };
        if n <= 0 {
            break;
        }
        let mut chunk = [0u8; 512];
        match stdin.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(k) => buf.extend_from_slice(&chunk[..k]),
        }
        if has_da1(&buf) {
            break;
        }
    }
    parse_reports(&buf)
}

fn has_da1(b: &[u8]) -> bool {
    b.windows(3).enumerate().any(|(i, w)| {
        w == b"\x1b[?" && {
            let rest = &b[i + 3..];
            let e = rest
                .iter()
                .position(|c| !(c.is_ascii_digit() || *c == b';'));
            e.is_some_and(|e| rest[e] == b'c')
        }
    })
}

/// The run loop asks before each wait; true once per requested re-probe.
pub fn take_reprobe(app: &mut App) -> bool {
    if !app.parity.appearance.want_probe {
        return false;
    }
    app.parity.appearance.want_probe = false;
    app.parity.appearance.last_probe = Some(Instant::now());
    true
}

/// The host window came back: the system appearance may have flipped meanwhile.
pub fn on_focus_gained(app: &mut App) {
    if !matches!(app.config.theme.mode, vk_config::ThemeMode::Auto) {
        return;
    }
    let st = &mut app.parity.appearance;
    if st.last_probe.is_none_or(|t| t.elapsed() >= REPROBE_MIN) {
        st.want_probe = true;
    }
}

/// A detection result (startup or re-probe): remember, report where it changed, re-theme.
pub fn on_detect(app: &mut App, det: Option<Detected>) {
    let Some(d) = det else {
        return;
    };
    app.parity.appearance.detected = Some(d);
    for mi in 0..app.machines.len() {
        report(app, mi);
    }
    apply(app, false);
}

/// A (re)connected server has no report from this client yet.
pub fn on_connected(app: &mut App, mi: usize) {
    app.parity.appearance.reported.remove(&mi);
    report(app, mi);
}

/// `client.appearance` to machine `mi` if it hasn't heard this value yet.
pub fn report(app: &mut App, mi: usize) {
    let Some(d) = app.parity.appearance.detected else {
        return;
    };
    if !app.machines[mi].connected() {
        // A reconnect reports again (`on_connected`).
        app.parity.appearance.reported.remove(&mi);
        return;
    }
    if app.parity.appearance.reported.get(&mi) == Some(&d.dark) {
        return;
    }
    app.parity.appearance.reported.insert(mi, d.dark);
    app.command_on(
        mi,
        "client.appearance",
        json!({"dark": d.dark, "source": d.source.as_str()}),
        Pending::Parity(Reply::Ignore),
    );
}

/// Which chrome theme to use: forced `theme.mode`, else this host's detection, else what the
/// server resolved; `auto_switch` picks `dark_name`/`light_name`, otherwise `name`.
pub fn theme_name(cfg: &vk_config::Config, local: Option<bool>, server: &Appearance) -> String {
    let th = &cfg.theme;
    let dark = match th.mode {
        vk_config::ThemeMode::Dark => Some(true),
        vk_config::ThemeMode::Light => Some(false),
        vk_config::ThemeMode::Auto => local.or(server.known.then_some(server.dark)),
    };
    match dark {
        Some(d) if th.auto_switch => {
            if d {
                th.dark_name.clone()
            } else {
                th.light_name.clone()
            }
        }
        _ => th.name.clone(),
    }
}

/// Re-theme the chrome if the effective theme changed (`force`: after a config reload).
pub fn apply(app: &mut App, force: bool) {
    let local = app.parity.appearance.detected.map(|d| d.dark);
    let name = theme_name(&app.config, local, &app.m().model.appearance);
    if !force && name == app.parity.appearance.applied {
        return;
    }
    app.theme = Theme::named(&name);
    app.parity.appearance.applied = name;
    app.prev = Grid::new(0, 0);
    app.dirty = true;
}

pub fn action(app: &mut App, action: &str) -> bool {
    if action != "theme_detect" {
        return false;
    }
    app.parity.appearance.want_probe = true;
    // Report again even when the value didn't change (e.g. the server restarted).
    app.parity.appearance.reported.clear();
    true
}
