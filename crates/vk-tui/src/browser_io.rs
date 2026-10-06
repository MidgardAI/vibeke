//! Browser pane page I/O on the client (06 B3.2): the console split toggle, page clipboard
//! writes (OSC 52 under the `clipboard` config), files and clipboard images into the page.
//!
//! - **Console split** (`browser_console`, `prefix+alt+c`): `browser.pane.console` on the
//!   pane's machine opens (or closes) a split under the browser pane running
//!   `vibeke browser console --pane <id> --follow`.
//! - **Page clipboard**: the media host sends `ServerFrame::Clipboard` for a browser pane; it is
//!   judged as a write from the pane's *owner* machine (`clipboard.osc52_write` for local
//!   panes, `clipboard.remote_write` ask-once for a remote machine's page) with the usual size
//!   limit. The host clipboard is never read into the page except by an explicit paste.
//! - **Files**: a bracketed paste that is only local file paths (a terminal drop) asks first;
//!   confirmed readable regular files ≤ 50 MiB go to the page (`BrowserCmd::DropFiles`): by
//!   path when the media host is this machine, else uploaded to its inbox first.
//! - **Clipboard image** (`browser_paste_image`, `prefix+shift+v`): read the host clipboard's
//!   PNG with the kitty clipboard protocol (OSC 5522), or the platform clipboard when the host
//!   terminal is on this machine, and hand it to the page the same way. Nothing is read
//!   without that key.

use crate::app::{App, Mode, Pending, Popup};
use crate::upload::{Item, Source};
use base64::Engine as _;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vk_proto::input::{Key, KeyEvent, NamedKey};
use vk_proto::render::{BrowserCmd, ClientFrame};

/// Largest file handed to a page, and files per drop (the server checks again).
pub const DROP_MAX: u64 = 50 << 20;
pub const DROP_FILES_MAX: usize = 16;

/// The drop confirmation (`Popup::BrowserDrop`).
#[derive(Debug, Clone)]
pub struct DropAsk {
    pub pane: String,
    /// The text as pasted (sent as text with `t`).
    pub original: String,
    /// Resolved local files and their sizes.
    pub files: Vec<(PathBuf, u64)>,
}

/// The machine whose server renders `pane` (where `BrowserCmd`s go).
fn host_of(app: &App, pane: &str) -> usize {
    app.browser
        .panes
        .get(pane)
        .map(|p| p.host)
        .unwrap_or_else(|| crate::browser::media_host(app, app.cur))
}

fn send(app: &mut App, host: usize, pane: &str, cmd: BrowserCmd) -> bool {
    let id = app.next_input;
    app.next_input += 1;
    app.machines[host].send(ClientFrame::Browser {
        input_id: id,
        pane: pane.to_string(),
        cmd,
    })
}

// ---- console split ----------------------------------------------------------------------------

/// `browser_console`: toggle the console/network split under the browser pane.
pub fn console_split(app: &mut App, pane: &str) {
    let owner = app.cur;
    app.command_on(
        owner,
        "browser.pane.console",
        json!({"pane": pane, "toggle": true}),
        Pending::Preview(crate::browser::Reply::ConsoleSplit),
    );
}

/// Reply to `browser.pane.console`.
pub fn on_console_reply(app: &mut App, v: &serde_json::Value) {
    if v.get("closed").is_some() {
        app.toast("console split closed");
    } else {
        app.toast(
            "console split: c console · n network · e errors only · a all · q quit (in the split)",
        );
    }
}

// ---- page clipboard ---------------------------------------------------------------------------

/// A clipboard write for `pane` from machine `i`. Browser panes are judged as writes from the
/// machine that owns the pane (its page); returns the data back when `pane` is not a browser
/// pane (the caller handles it as an ordinary OSC 52 write).
pub fn on_clipboard(app: &mut App, i: usize, pane: &str, data: Vec<u8>) -> Option<Vec<u8>> {
    let owner = app.machines.iter().position(|m| {
        m.model
            .panes
            .iter()
            .any(|p| p.id == pane && p.browser.is_some())
    });
    match owner {
        Some(owner) => {
            let _ = i;
            app.on_clipboard(owner, pane.to_string(), false, data);
            None
        }
        None => Some(data),
    }
}

// ---- files into the page ----------------------------------------------------------------------

/// Why a local path can't go into a page (`None` = it can).
pub fn drop_problem(p: &Path) -> Option<String> {
    match std::fs::metadata(p) {
        Err(e) => Some(e.to_string()),
        Ok(m) if !m.is_file() => Some("not a regular file".into()),
        Ok(m) if m.len() > DROP_MAX => Some(format!("larger than {} MiB", DROP_MAX >> 20)),
        Ok(_) => std::fs::File::open(p)
            .err()
            .map(|e| format!("not readable: {e}")),
    }
}

/// A paste into a focused browser pane: only local file paths → ask before handing the files
/// to the page. Returns false when the paste is ordinary text.
pub fn maybe_drop(app: &mut App, pane: &str, text: &str) -> bool {
    let Some(parsed) = crate::paste::parse_paste(text) else {
        return false;
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut files = Vec::new();
    for t in &parsed.tokens {
        let p = t.local_path(&home);
        // Paths that don't exist (or are directories) are just text for the page.
        let Ok(m) = std::fs::metadata(&p) else {
            return false;
        };
        if !m.is_file() {
            return false;
        }
        if let Some(why) = drop_problem(&p) {
            app.toast(format!(
                "{} can't go into the page ({why}) — pasted as text",
                t.basename()
            ));
            return false;
        }
        files.push((std::fs::canonicalize(&p).unwrap_or(p), m.len()));
    }
    if files.len() > DROP_FILES_MAX {
        app.toast(format!(
            "at most {DROP_FILES_MAX} files at once — pasted as text"
        ));
        return false;
    }
    app.mode = Mode::Popup(Popup::BrowserDrop(Box::new(DropAsk {
        pane: pane.to_string(),
        original: text.to_string(),
        files,
    })));
    true
}

/// Hand confirmed files to the page: by path to a media host on this machine, else uploaded
/// to the media host's inbox first (the transfer then sends the inbox paths).
pub fn drop_files(app: &mut App, pane: &str, files: Vec<(PathBuf, u64)>) {
    let host = host_of(app, pane);
    if app.machines[host].local {
        let paths = files
            .iter()
            .map(|(p, _)| p.to_string_lossy().into_owned())
            .collect();
        if !send(app, host, pane, BrowserCmd::DropFiles(paths)) {
            app.toast(format!(
                "{} offline — not dropped",
                app.machines[host].label
            ));
        }
        return;
    }
    let items = files
        .into_iter()
        .map(|(p, size)| Item {
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into()),
            size,
            src: Source::Path(p),
        })
        .collect();
    crate::upload::begin_browser(app, host, pane, items);
}

/// Keys in the drop confirmation: `d`/enter drop, `t` paste as text, esc/`c` cancel; other keys
/// are ignored (nothing happens by accident).
pub fn drop_key(app: &mut App, ev: KeyEvent, ask: Box<DropAsk>) {
    match ev.key {
        Key::Char('d' | 'D') | Key::Named(NamedKey::Enter) => {
            let DropAsk { pane, files, .. } = *ask;
            drop_files(app, &pane, files);
        }
        Key::Char('t' | 'T') => {
            let host = host_of(app, &ask.pane);
            send(app, host, &ask.pane, BrowserCmd::Text(ask.original.clone()));
        }
        Key::Char('c' | 'C') | Key::Named(NamedKey::Escape) => app.toast("not dropped"),
        _ => app.mode = Mode::Popup(Popup::BrowserDrop(ask)),
    }
}

pub fn draw_drop(app: &App, g: &mut crate::screen::Grid, ask: &DropAsk) {
    let t = &app.theme;
    let shown = ask.files.len().min(6);
    let mut b = crate::popups::frame(app, g, 72, (shown + 6) as u16, "files into the page");
    let total: u64 = ask.files.iter().map(|(_, s)| s).sum();
    b.line(
        &format!(
            "Give {} file(s) ({}) to the page in this browser pane?",
            ask.files.len(),
            crate::upload::human(total)
        ),
        t.text(),
    );
    for (p, s) in ask.files.iter().take(6) {
        b.line(
            &format!(
                "  {}  {}",
                p.file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default(),
                crate::upload::human(*s)
            ),
            t.dim(),
        );
    }
    if ask.files.len() > 6 {
        b.line(&format!("  … and {} more", ask.files.len() - 6), t.dim());
    }
    b.line(
        "[d] drop into the page   [t] paste the path as text   [esc] cancel",
        t.text(),
    );
    b.line(
        "(an open file chooser in the page gets them; else they are dropped)",
        t.dim(),
    );
}

// ---- clipboard image (prefix+shift+v) ---------------------------------------------------------

/// `browser_paste_image`: read the host clipboard image after this frame (the event reader is
/// stopped while the terminal answers).
pub fn request_image_paste(app: &mut App, pane: &str) {
    app.browser.clip_read = Some(pane.to_string());
    app.dirty = true;
}

/// The main loop's turn: a pending clipboard-image read, if any.
pub fn take_clip_read(app: &mut App) -> Option<String> {
    app.browser.clip_read.take()
}

/// The image the read produced (`None` = nothing usable): hand it to the page.
pub fn on_clip_image(app: &mut App, pane: &str, img: Option<(String, Vec<u8>)>) {
    let img = img.or_else(|| {
        // The platform clipboard is this machine's: only right when the host terminal is too.
        if app.caps.host_remote || cfg!(test) {
            None
        } else {
            crate::clipboard::os_clipboard_image().ok().flatten()
        }
    });
    let Some((mime, data)) = img else {
        app.toast("no image on the clipboard (or the terminal doesn't share it) — drop a file path instead");
        return;
    };
    if data.len() as u64 > DROP_MAX {
        app.toast(format!("clipboard image is over {} MiB", DROP_MAX >> 20));
        return;
    }
    let ext = match mime.as_str() {
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    };
    let name = format!(
        "clipboard-{}.{ext}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let host = host_of(app, pane);
    app.toast(format!(
        "pasting a clipboard image ({}) into the page",
        crate::upload::human(data.len() as u64)
    ));
    crate::upload::begin_browser(
        app,
        host,
        pane,
        vec![Item {
            name,
            size: data.len() as u64,
            src: Source::Bytes(Arc::new(data)),
        }],
    );
}

/// OSC 5522 read request for `mime` (kitty clipboard protocol). With `sentinel`, DA1 follows
/// so a terminal that ignores the request is noticed at once.
pub fn osc5522_request(mime: &str, sentinel: bool) -> Vec<u8> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(mime);
    let mut v = format!("\x1b]5522;type=read;{b64}\x1b\\").into_bytes();
    if sentinel {
        v.extend_from_slice(b"\x1b[c");
    }
    v
}

/// kitty speaks OSC 5522 and may ask the user before answering (no DA1 sentinel then).
pub fn host_is_kitty() -> bool {
    std::env::var("TERM").is_ok_and(|t| t.contains("kitty"))
        || std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var("TERM_PROGRAM").is_ok_and(|t| t == "kitty")
}

/// What the terminal said so far.
#[derive(Debug, PartialEq)]
pub enum Clip5522 {
    /// Still waiting; `seen` = at least one 5522 reply arrived (the terminal speaks it).
    Pending { seen: bool },
    /// Finished: data per MIME type.
    Done(Vec<(String, Vec<u8>)>),
    /// The terminal refused (`EPERM`, `ENOSYS`, …) or doesn't speak 5522 (DA1 came first).
    Failed(String),
}

/// Parse replies in `buf`: `ESC ] 5522 ; type=read:status=OK|DATA|DONE|E… [:mime=<b64>] ;
/// <b64 payload> ST` messages (BEL or ST terminated), and a DA1 reply (`ESC [ ? … c`).
pub fn parse_5522(buf: &[u8]) -> Clip5522 {
    let s = String::from_utf8_lossy(buf);
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let mut seen = false;
    let mut rest: &str = &s;
    let da1_at = s.find("\x1b[?").filter(|i| s[*i..].find('c').is_some());
    while let Some(i) = rest.find("\x1b]5522;") {
        let body_start = i + "\x1b]5522;".len();
        let tail = &rest[body_start..];
        let end = match (tail.find("\x1b\\"), tail.find('\x07')) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break, // incomplete message
        };
        seen = true;
        let msg = &tail[..end];
        let (meta, payload) = msg.split_once(';').unwrap_or((msg, ""));
        let mut status = "";
        let mut mime = String::new();
        for kv in meta.split(':') {
            match kv.split_once('=') {
                Some(("status", v)) => status = v,
                Some(("mime", v)) => {
                    mime = base64::engine::general_purpose::STANDARD
                        .decode(v)
                        .ok()
                        .and_then(|b| String::from_utf8(b).ok())
                        .filter(|m| m.contains('/') || m == ".")
                        .unwrap_or_else(|| v.to_string());
                }
                _ => {}
            }
        }
        match status {
            "OK" => {}
            "DATA" => {
                let clean: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(clean.as_bytes())
                    .unwrap_or_default();
                match out.iter_mut().find(|(m, _)| *m == mime) {
                    Some((_, d)) => d.extend_from_slice(&bytes),
                    None => out.push((mime, bytes)),
                }
            }
            "DONE" => return Clip5522::Done(out),
            e if e.starts_with('E') => return Clip5522::Failed(e.to_string()),
            _ => {}
        }
        rest = &tail[end..];
    }
    if !seen && da1_at.is_some() {
        return Clip5522::Failed("the terminal does not support OSC 5522".into());
    }
    Clip5522::Pending { seen }
}

/// Ask the host terminal for its clipboard PNG over OSC 5522 (the TUI's event reader must be
/// stopped). Gives up quickly when the terminal answers DA1 without a 5522 reply; a terminal
/// that speaks it may ask the user first, so it gets longer.
pub fn osc5522_read() -> Option<(String, Vec<u8>)> {
    use std::io::{Read, Write};
    if cfg!(test) {
        return None;
    }
    let mut out = std::io::stdout();
    let kitty = host_is_kitty();
    out.write_all(&osc5522_request("image/png", !kitty)).ok()?;
    out.flush().ok()?;
    let started = Instant::now();
    let mut buf = Vec::new();
    let mut stdin = std::io::stdin();
    loop {
        let limit = match parse_5522(&buf) {
            Clip5522::Done(v) => {
                return v
                    .into_iter()
                    .find(|(m, d)| m.starts_with("image/") && !d.is_empty());
            }
            Clip5522::Failed(_) => return None,
            Clip5522::Pending { seen: true } => Duration::from_secs(20),
            Clip5522::Pending { seen: false } if kitty => Duration::from_secs(20),
            Clip5522::Pending { seen: false } => Duration::from_secs(2),
        };
        let left = limit.saturating_sub(started.elapsed());
        if left.is_zero() || buf.len() > 80 << 20 {
            return None;
        }
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: polling stdin with a valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(500) as i32) };
        if n < 0 {
            return None;
        }
        if n == 0 {
            continue;
        }
        let mut chunk = [0u8; 64 * 1024];
        match stdin.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(k) => buf.extend_from_slice(&chunk[..k]),
        }
    }
}

#[cfg(test)]
#[path = "browser_io_tests.rs"]
mod tests;
