//! Browser pane page I/O on the client (06 B3.2): the console split toggle, page clipboard
//! writes (OSC 52 under the `clipboard` config), files and clipboard images into the page.
//!
//! - **Console split** (`browser_console`, `prefix+alt+c`): `browser.pane.console` on the
//!   pane's machine opens (or closes) a split under the browser pane running
//!   `vibeke browser console --pane <id> --follow`.
//! - **Page clipboard**: only the machine that renders the page (the pane's media host) may send
//!   `ServerFrame::Clipboard` for a browser pane; a frame naming a browser pane from any other
//!   machine is dropped. It is judged as a write from a remote machine when the sender or the
//!   pane's owner is remote (`clipboard.remote_write`, ask once for that machine), else under
//!   `clipboard.osc52_write`, with the usual size limit. The host clipboard is never read into
//!   the page except by an explicit paste.
//! - **Files**: a bracketed paste that is only local file paths (a terminal drop) asks first,
//!   recording each file's identity (device, inode, size). On confirm each file is opened with
//!   `O_NOFOLLOW` and must still be that file, a regular file ≤ 50 MiB; the bytes are read from
//!   that descriptor and uploaded to the media host (`blob.* {stage: "browser"}`, also when it
//!   is this machine), which keeps the copy in its private drop directory and hands the page
//!   that copy (`BrowserCmd::DropFiles`). A file that is swapped, grows or shrinks after the
//!   prompt is refused.
//! - **Clipboard image** (`browser_paste_image`, `prefix+shift+v`): the platform clipboard's
//!   PNG when the host terminal is on this machine, handed to the page the same way. Nothing is
//!   read without that key. The kitty clipboard protocol (OSC 5522) is not used: its reply
//!   arrives on stdin, which crossterm owns, and a reply after a timeout (kitty may ask the
//!   user first) would be parsed as keystrokes for the focused pane; filtering it needs a raw
//!   input layer in front of crossterm, which the TUI doesn't have.

use crate::app::{App, Mode, Pending, Popup};
use crate::upload::{Item, Source};
use serde_json::json;
#[cfg(not(target_arch = "wasm32"))]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use vk_proto::input::{Key, KeyEvent, NamedKey};
use vk_proto::render::{BrowserCmd, ClientFrame};

/// Largest file handed to a page, and files per drop (the server checks again).
pub const DROP_MAX: u64 = 50 << 20;
pub const DROP_FILES_MAX: usize = 16;

/// A file the drop prompt shows: its path (symlinks resolved when asked) and identity then.
#[derive(Debug, Clone, PartialEq)]
pub struct DropFile {
    pub path: PathBuf,
    pub size: u64,
    pub dev: u64,
    pub ino: u64,
}

impl DropFile {
    /// The file at `path` now (following links), if it is a regular file.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn at(path: &Path) -> std::io::Result<DropFile> {
        let canon = std::fs::canonicalize(path)?;
        let m = std::fs::metadata(&canon)?;
        if !m.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        Ok(DropFile {
            path: canon,
            size: m.len(),
            dev: m.dev(),
            ino: m.ino(),
        })
    }

    fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into())
    }
}

/// The drop confirmation (`Popup::BrowserDrop`).
#[derive(Debug, Clone)]
pub struct DropAsk {
    pub pane: String,
    /// The text as pasted (sent as text with `t`).
    pub original: String,
    /// The local files, as they were when the prompt opened.
    pub files: Vec<DropFile>,
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

/// A clipboard write for `pane` from machine `i`. For a browser pane it is accepted only from
/// the pane's media host (the machine rendering its page, which is where page clipboard frames
/// come from) and judged as a write from a remote machine — the sender, else the pane's owner —
/// when either is remote. Returns the data back when `pane` is not a browser pane (the caller
/// handles it as machine `i`'s ordinary OSC 52 write).
pub fn on_clipboard(app: &mut App, i: usize, pane: &str, data: Vec<u8>) -> Option<Vec<u8>> {
    let Some(owner) = app.machines.iter().position(|m| {
        m.model
            .panes
            .iter()
            .any(|p| p.id == pane && p.browser.is_some())
    }) else {
        return Some(data);
    };
    if !crate::browser::renders(app, i, pane) {
        // Another machine naming a browser pane it doesn't render: not its page's write.
        app.clip.dropped += 1;
        return None;
    }
    let judge = if !app.machines[i].local { i } else { owner };
    app.on_clipboard(judge, pane.to_string(), false, data);
    None
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

/// Open a confirmed file for delivery: the last component must not be a link (`O_NOFOLLOW`),
/// and the descriptor must be the very file the prompt showed (device, inode and size), a
/// regular file ≤ 50 MiB. The upload then reads only from this descriptor.
#[cfg(not(target_arch = "wasm32"))]
pub fn open_snapshot(f: &DropFile) -> Result<std::fs::File, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&f.path)
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) => "replaced by a link since you confirmed it".to_string(),
            _ => e.to_string(),
        })?;
    let m = file.metadata().map_err(|e| e.to_string())?;
    if !m.is_file() {
        return Err("not a regular file".into());
    }
    if m.len() > DROP_MAX {
        return Err(format!("larger than {} MiB", DROP_MAX >> 20));
    }
    if (m.dev(), m.ino()) != (f.dev, f.ino) {
        return Err("replaced since you confirmed it".into());
    }
    if m.len() != f.size {
        return Err("changed size since you confirmed it".into());
    }
    Ok(file)
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
        match DropFile::at(&p) {
            Ok(f) => files.push(f),
            Err(_) => return false,
        }
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

/// Hand confirmed files to the page: each opened and checked ([`open_snapshot`]), its bytes
/// uploaded from that descriptor to the media host's drop directory (this machine's server
/// too), then dropped from there. Nothing is sent when any file fails the check.
pub fn drop_files(app: &mut App, pane: &str, files: Vec<DropFile>) {
    let host = host_of(app, pane);
    let mut items = Vec::new();
    for f in &files {
        match open_snapshot(f) {
            Ok(file) => items.push(Item {
                name: f.name(),
                size: f.size,
                src: Source::File(Arc::new(file)),
            }),
            Err(why) => {
                app.toast(format!("{}: {why} — not dropped", f.name()));
                return;
            }
        }
    }
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
    let total: u64 = ask.files.iter().map(|f| f.size).sum();
    b.line(
        &format!(
            "Give {} file(s) ({}) to the page in this browser pane?",
            ask.files.len(),
            crate::upload::human(total)
        ),
        t.text(),
    );
    for f in ask.files.iter().take(6) {
        b.line(
            &format!("  {}  {}", f.name(), crate::upload::human(f.size)),
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

/// `browser_paste_image`: the platform clipboard's image, only when the host terminal is on
/// this machine (its clipboard is this machine's); see the module docs for why OSC 5522 isn't
/// used.
pub fn request_image_paste(app: &mut App, pane: &str) {
    let img = if app.caps.host_remote || cfg!(test) {
        None
    } else {
        crate::clipboard::os_clipboard_image().ok().flatten()
    };
    on_clip_image(app, pane, img);
}

/// The image the read produced (`None` = nothing usable): hand it to the page.
pub fn on_clip_image(app: &mut App, pane: &str, img: Option<(String, Vec<u8>)>) {
    let Some((mime, data)) = img else {
        if app.caps.host_remote {
            app.toast("the terminal is on another machine and its clipboard can't be read here — drop a file path instead");
        } else {
            app.toast("no image on the clipboard — drop a file path instead");
        }
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
        crate::time::SystemTime::now()
            .duration_since(crate::time::UNIX_EPOCH)
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

#[cfg(test)]
#[path = "browser_io_tests.rs"]
mod tests;

#[cfg(target_arch = "wasm32")]
impl DropFile {
    pub fn at(_: &Path) -> std::io::Result<DropFile> {
        Err(std::io::Error::other(
            "Local file uploads are unavailable in the browser TUI",
        ))
    }
}
#[cfg(target_arch = "wasm32")]
pub fn open_snapshot(_: &DropFile) -> Result<std::fs::File, String> {
    Err("Local file uploads are unavailable in the browser TUI".into())
}
