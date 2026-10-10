//! Client side of the terminal effects (03 §8):
//!
//! - **OSC 52 read**: a pane asked for the clipboard (`ServerFrame::ClipboardQuery`). The
//!   client applies `clipboard.osc52_read` on the machine the user sits at: `deny` (default)
//!   answers "denied" and toasts, `allow` reads the clipboard and toasts what was shared,
//!   `ask` queues a non-modal notice; `prefix+y` (`review_clipboard`) opens a prompt naming
//!   the pane (`y` once, `a` always for this pane this session, `n`/`esc` deny). A read is
//!   never answered without the user seeing it. Over SSH without `VIBEKE_CLIPBOARD_READ_CMD`
//!   the clipboard here is not the user's, so reads are denied.
//! - **OSC 8 links**: `ctrl`/`alt`+hover underlines the whole link (every run with that target
//!   in the pane, wrapped pieces included); `ctrl`/`alt`+click opens it (`nav::open_url`:
//!   loopback URLs as a browser pane on the pane's machine, other http(s) with the OS opener,
//!   anything else is copied) after plugin link handlers had their chance. On macOS, Cmd+click
//!   is the host terminal's own (links are passed through with OSC 8). Plain-text URLs are the
//!   fallback ([`plain_url_at`]).
//! - **OSC 9;4 progress** and the **last exit code**: small sidebar / tab-bar segments and a
//!   5 s exit badge ([`progress_bar`], [`exit_badge`]).

use crate::app::{App, Mode, Popup};
use crate::event::{KeyModifiers, MouseEvent, MouseEventKind};
use crate::screen::Grid;
use crate::time::{Duration, Instant};
use std::collections::{HashMap, HashSet};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::model::{Progress, ProgressState};
use vk_proto::render::{ClientFrame, ClipSel, Style, attr};

/// How long a non-zero exit code shows after it arrived.
pub const EXIT_BADGE: Duration = Duration::from_secs(5);
/// Queued clipboard-read prompts (the oldest is denied when more arrive).
const READ_QUEUE_MAX: usize = 4;
/// Largest clipboard read sent to a pane.
pub const READ_MAX: usize = 1 << 20;

/// An OSC 52 read waiting for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadReq {
    pub machine: usize,
    pub req: u64,
    pub pane: String,
    pub primary: bool,
}

#[derive(Default)]
pub struct State {
    /// Replaces the platform paste tool (`VIBEKE_CLIPBOARD_READ_CMD`, a `sh -c` command line
    /// whose stdout is the clipboard); a test hook like `VIBEKE_CLIPBOARD_CMD`.
    pub read_cmd: Option<String>,
    /// Reads waiting for `review_clipboard`.
    pub reads: Vec<ReadReq>,
    /// Panes allowed to read for the rest of this TUI run (`a` in the prompt).
    allowed: HashSet<(usize, String)>,
    /// Tests only: the clipboard content instead of running a tool.
    pub test_clipboard: Option<Vec<u8>>,
    /// The link under the pointer while `ctrl`/`alt` is held: (machine, pane, target).
    pub hover: Option<(usize, String, String)>,
    /// When each pane's current exit mark was first seen here (server clocks may differ).
    exit_seen: HashMap<(usize, String), (i64, Instant)>,
    /// When each recovered pane's transient "reconnected" notice was first seen here, and
    /// whether a keypress in the pane already dismissed it.
    recovery_seen: HashMap<(usize, String), (Instant, bool)>,
}

impl State {
    pub fn from_env() -> Self {
        State {
            read_cmd: std::env::var("VIBEKE_CLIPBOARD_READ_CMD")
                .ok()
                .filter(|c| !c.trim().is_empty()),
            ..Default::default()
        }
    }

    /// Tests: reads return `clipboard` and never run a tool.
    pub fn with_test_clipboard(clipboard: &[u8]) -> Self {
        State {
            test_clipboard: Some(clipboard.to_vec()),
            ..Default::default()
        }
    }
}

/// `w1:p3 "title"` for prompts and toasts.
fn pane_label(app: &App, mi: usize, pane: &str) -> String {
    let m = &app.machines[mi];
    let p = m.model.panes.iter().find(|p| p.id == pane);
    let name = p.map(|p| {
        let run = m.model.runs.iter().find(|r| r.pane == p.id);
        match run {
            Some(r) => r.label().to_string(),
            None => p.display_title().to_string(),
        }
    });
    let handle = p.map(|p| p.handle.as_str()).unwrap_or(pane);
    let mut s = match name.filter(|n| !n.is_empty()) {
        Some(n) => format!("{handle} \"{}\"", crate::draw::truncate(&n, 24)),
        None => handle.to_string(),
    };
    if !m.local {
        s = format!("{} {s}", m.label);
    }
    s
}

fn reply(app: &App, mi: usize, req: u64, pane: &str, data: Option<Vec<u8>>) {
    app.machines[mi].send(ClientFrame::ClipboardReply {
        req,
        pane: pane.to_string(),
        data,
    });
}

/// `ServerFrame::ClipboardQuery`.
pub fn on_query(app: &mut App, mi: usize, req: u64, pane: String, sel: ClipSel) {
    let primary = matches!(sel, ClipSel::Primary);
    let label = pane_label(app, mi, &pane);
    match app.config.clipboard.osc52_read {
        vk_config::Osc52Read::Deny => {
            reply(app, mi, req, &pane, None);
            app.toast(format!(
                "⎘ {label} tried to read your clipboard — denied (clipboard.osc52_read = deny)"
            ));
        }
        vk_config::Osc52Read::Allow => grant(app, mi, req, &pane, primary),
        vk_config::Osc52Read::Ask => {
            if app.osc.allowed.contains(&(mi, pane.clone())) {
                grant(app, mi, req, &pane, primary);
                return;
            }
            app.osc
                .reads
                .retain(|r| !(r.machine == mi && r.pane == pane));
            if app.osc.reads.len() >= READ_QUEUE_MAX {
                let old = app.osc.reads.remove(0);
                reply(app, old.machine, old.req, &old.pane, None);
            }
            app.osc.reads.push(ReadReq {
                machine: mi,
                req,
                pane,
                primary,
            });
            app.toast(format!(
                "⎘ {label} wants to read your clipboard — prefix+y to review"
            ));
        }
    }
}

/// Read the clipboard (or primary selection) of this machine: the override command, else
/// the platform tool.
fn read_clipboard(app: &App, primary: bool) -> anyhow::Result<Vec<u8>> {
    if let Some(d) = &app.osc.test_clipboard {
        return Ok(d.clone());
    }
    if let Some(cmd) = &app.osc.read_cmd {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .env(
                "VIBEKE_CLIPBOARD_SELECTION",
                if primary { "primary" } else { "clipboard" },
            )
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()?;
        anyhow::ensure!(out.status.success(), "VIBEKE_CLIPBOARD_READ_CMD failed");
        return Ok(out.stdout);
    }
    anyhow::ensure!(
        !app.copyout.env.ssh,
        "over SSH the clipboard here is not yours"
    );
    crate::clipboard::os_paste(primary)
}

/// Answer a read with the clipboard and say so.
fn grant(app: &mut App, mi: usize, req: u64, pane: &str, primary: bool) {
    let label = pane_label(app, mi, pane);
    match read_clipboard(app, primary) {
        Ok(mut data) => {
            data.truncate(READ_MAX);
            let n = data.len();
            reply(app, mi, req, pane, Some(data));
            app.toast(format!("⎘ shared your clipboard ({n} bytes) with {label}"));
        }
        Err(e) => {
            reply(app, mi, req, pane, None);
            app.toast(format!("⎘ clipboard read by {label} failed — {e}"));
        }
    }
}

/// `review_clipboard`: open the oldest pending read. False when there is none.
pub fn review_read(app: &mut App) -> bool {
    if app.osc.reads.is_empty() {
        return false;
    }
    let r = app.osc.reads.remove(0);
    app.mode = Mode::Popup(Popup::ClipboardRead(r));
    true
}

/// Keys in the read prompt: only explicit answers act.
pub fn read_key(app: &mut App, ev: KeyEvent, r: ReadReq) {
    if ev.kind == KeyKind::Release {
        app.mode = Mode::Popup(Popup::ClipboardRead(r));
        return;
    }
    match ev.key {
        Key::Char('y' | 'Y') => grant(app, r.machine, r.req, &r.pane, r.primary),
        Key::Char('a' | 'A') => {
            app.osc.allowed.insert((r.machine, r.pane.clone()));
            grant(app, r.machine, r.req, &r.pane, r.primary);
        }
        Key::Char('n' | 'N') | Key::Named(NamedKey::Escape) => {
            reply(app, r.machine, r.req, &r.pane, None);
            let label = pane_label(app, r.machine, &r.pane);
            app.toast(format!("⎘ clipboard read by {label} denied"));
        }
        _ => app.mode = Mode::Popup(Popup::ClipboardRead(r)),
    }
}

pub fn draw_read(app: &App, g: &mut Grid, r: &ReadReq) {
    let t = app.theme;
    let mut b = crate::popups::frame(app, g, 70, 7, "clipboard read");
    b.line(
        &format!(
            "{} wants to READ your {}.",
            pane_label(app, r.machine, &r.pane),
            if r.primary {
                "primary selection"
            } else {
                "clipboard"
            }
        ),
        t.bold(t.yellow),
    );
    b.line("The program would see whatever you copied last.", t.text());
    b.line(
        "[y] allow once   [a] allow this pane until detach   [n/esc] deny",
        t.dim(),
    );
}

// ---- links --------------------------------------------------------------------------------

/// The pane under host cell (`x`, `y`) with pane-local coordinates, for terminal panes only.
fn pane_at(app: &App, x: u16, y: u16) -> Option<(String, u16, u16)> {
    let (pane, r) = app
        .pane_rects()
        .into_iter()
        .find(|(_, r)| r.contains(x, y))?;
    if crate::browser::browser_of(app, app.cur, &pane).is_some() {
        return None;
    }
    Some((pane, x - r.x, y - r.y))
}

/// The OSC 8 target at a pane-local cell.
pub fn link_at(app: &App, mi: usize, pane: &str, col: u16, row: u16) -> Option<String> {
    let buf = app.machines[mi].panes.get(pane)?;
    let line = buf.lines.get(row as usize)?;
    line.link_at(col).map(|l| l.uri.clone())
}

/// Plain-text `http(s)://` URL under a pane-local cell (the linkifier fallback).
pub fn plain_url_at(app: &App, mi: usize, pane: &str, col: u16, row: u16) -> Option<String> {
    let tok = crate::plugins::token_at(app, mi, pane, col, row)?;
    let start = tok.find("http://").or_else(|| tok.find("https://"))?;
    let url = tok[start..].trim_end_matches(['.', ',', ';', ':', ')', ']', '>', '"', '\'']);
    (url.len() > "https://".len() && url.len() <= vk_term::limits::LINK_URI_MAX)
        .then(|| url.to_string())
}

/// Open a link target from a pane: plugin handlers first, then `nav::open_url` for safe
/// targets (http/https; loopback as a browser pane), else copy.
pub fn activate(app: &mut App, mi: usize, pane: &str, uri: &str) {
    if uri.chars().any(|c| c.is_control()) || uri.len() > vk_term::limits::LINK_URI_MAX {
        app.toast("link not opened: unsafe target");
        return;
    }
    let web = uri.starts_with("http://") || uri.starts_with("https://");
    if crate::plugins::offer_link(app, mi, pane, uri, web) {
        return;
    }
    if web {
        crate::nav::open_url(app, mi, pane, uri);
    } else {
        // file:, mailto:, custom schemes: never launched from a pane; copied instead.
        app.set_clipboard(uri.as_bytes(), false);
        let scheme = uri.split(':').next().unwrap_or("");
        app.toast(format!("{scheme}: links are copied, not opened"));
    }
}

fn modded(me: &MouseEvent) -> bool {
    me.modifiers.contains(KeyModifiers::CONTROL) || me.modifiers.contains(KeyModifiers::ALT)
}

/// Pointer events: track the hovered link while `ctrl`/`alt` is held, and open an OSC 8 link
/// on `ctrl`/`alt`+click. True when the event was consumed.
pub fn on_mouse(app: &mut App, me: &MouseEvent) -> bool {
    let hit = pane_at(app, me.column, me.row);
    let cur = app.cur;
    let hover = match (&hit, modded(me)) {
        (Some((pane, c, r)), true) => {
            link_at(app, cur, pane, *c, *r).map(|u| (cur, pane.clone(), u))
        }
        _ => None,
    };
    if hover != app.osc.hover {
        app.osc.hover = hover;
        app.dirty = true;
    }
    if let (MouseEventKind::Down(crate::event::MouseButton::Left), true, Some((pane, c, r))) =
        (me.kind, modded(me), hit)
        && let Some(uri) = link_at(app, cur, &pane, c, r)
    {
        activate(app, cur, &pane, &uri);
        return true;
    }
    false
}

/// Underline the hovered link's cells in a drawn pane (`r` = the pane rect).
pub fn draw_hover(app: &App, g: &mut Grid, mi: usize, pane: &str, r: vk_proto::layout::Rect) {
    let Some((hm, hp, uri)) = &app.osc.hover else {
        return;
    };
    if *hm != mi || hp != pane {
        return;
    }
    let Some(buf) = app.machines[mi].panes.get(pane) else {
        return;
    };
    for (y, row) in buf.lines.iter().enumerate().take(r.h as usize) {
        for l in row.links.iter().filter(|l| &l.uri == uri) {
            if l.col < r.w {
                g.add_attrs(
                    r.x + l.col,
                    r.y + y as u16,
                    l.cols.min(r.w - l.col),
                    attr::UNDERLINE,
                );
            }
        }
    }
}

// ---- progress and exit codes ---------------------------------------------------------------

/// A 4-cell OSC 9;4 bar (`▰▰▱▱`), `⋯` busy for indeterminate, with its colour.
pub fn progress_bar(app: &App, p: &Progress) -> (String, Style) {
    let t = app.theme;
    let color = match p.state {
        ProgressState::Error => t.red,
        ProgressState::Paused => t.yellow,
        _ => t.accent,
    };
    let s = match (p.state, p.pct) {
        (ProgressState::Indeterminate, _) | (_, None) => "⋯".to_string(),
        (_, Some(pct)) => {
            let full = ((pct as usize) * 4 + 50) / 100;
            format!("{}{}", "▰".repeat(full), "▱".repeat(4 - full.min(4)))
        }
    };
    (s, t.s(color))
}

/// The progress to show for the panes in `ids` of machine `mi` (sidebar rows, tabs): an error
/// first, else the furthest along.
pub fn progress_in<'a>(app: &'a App, mi: usize, ids: &[&str]) -> Option<&'a Progress> {
    let m = &app.machines[mi];
    m.model
        .pane_live
        .iter()
        .filter(|l| ids.contains(&l.pane.as_str()))
        .filter_map(|l| l.progress.as_ref())
        .max_by_key(|p| (p.state == ProgressState::Error, p.pct))
}

/// A new model from machine `mi`: note when each exit mark was first seen.
pub fn on_model(app: &mut App, mi: usize) {
    let now = Instant::now();
    let rec: Vec<String> = app.machines[mi]
        .model
        .panes
        .iter()
        .filter(|p| matches!(p.recovered.as_deref(), Some(m) if m != "lost"))
        .map(|p| p.id.clone())
        .collect();
    app.osc
        .recovery_seen
        .retain(|(m, p), _| *m != mi || rec.contains(p));
    for p in rec {
        app.osc.recovery_seen.entry((mi, p)).or_insert((now, false));
    }
    let marks: Vec<(String, i64)> = app.machines[mi]
        .model
        .pane_live
        .iter()
        .filter_map(|l| l.last_exit.map(|e| (l.pane.clone(), e.at_ms)))
        .collect();
    app.osc
        .exit_seen
        .retain(|(m, p), _| *m != mi || marks.iter().any(|(q, _)| q == p));
    for (pane, at) in marks {
        let e = app.osc.exit_seen.entry((mi, pane)).or_insert((at, now));
        if e.0 != at {
            *e = (at, now);
        }
    }
}

/// ` ✗ exit 3 ` while a pane's last command failed less than [`EXIT_BADGE`] ago.
pub fn exit_badge(app: &App, mi: usize, pane: &str, now: Instant) -> Option<String> {
    let live = app.machines[mi].model.live(pane)?;
    let e = live.last_exit?;
    let (at, seen) = app.osc.exit_seen.get(&(mi, pane.to_string()))?;
    (*at == e.at_ms && now.duration_since(*seen) < EXIT_BADGE)
        .then(|| format!(" ✗ exit {} ", e.code))
}

/// How long the "reconnected after server restart" notice shows.
pub const RECOVERY_NOTICE: Duration = Duration::from_secs(5);

/// The recovery notice of a pane: a transient " reconnected after server restart " (gone
/// after [`RECOVERY_NOTICE`] or the next keypress in the pane), or the persistent
/// " earlier output lost " when the holder's ring no longer held the pane's whole history.
pub fn recovery_badge(
    app: &App,
    mi: usize,
    pane: &str,
    recovered: &str,
    now: Instant,
) -> Option<&'static str> {
    if recovered == "lost" {
        return Some(" earlier output lost ");
    }
    let (seen, dismissed) = app.osc.recovery_seen.get(&(mi, pane.to_string()))?;
    (!dismissed && now.duration_since(*seen) < RECOVERY_NOTICE)
        .then_some(" reconnected after server restart ")
}

/// A keypress went to the focused pane: its transient recovery notice goes away.
pub fn dismiss_recovery(app: &mut App) {
    if let Some(p) = app.focused_pane() {
        let cur = app.cur;
        if let Some(e) = app.osc.recovery_seen.get_mut(&(cur, p)) {
            e.1 = true;
        }
    }
}

/// Redraw when an exit badge expires.
pub fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    if let Some(t) = app
        .osc
        .exit_seen
        .values()
        .map(|(_, seen)| *seen + EXIT_BADGE)
        .filter(|t| *t > now)
        .min()
    {
        d.at("exit_badge", t);
    }
    if let Some(t) = app
        .osc
        .recovery_seen
        .values()
        .filter(|(_, dismissed)| !dismissed)
        .map(|(seen, _)| *seen + RECOVERY_NOTICE)
        .filter(|t| *t > now)
        .min()
    {
        d.at("recovery_notice", t);
    }
}

#[cfg(test)]
#[path = "osc_tests.rs"]
mod tests;
