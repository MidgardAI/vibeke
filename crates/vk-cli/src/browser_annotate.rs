//! `vibeke browser annotations --pane <p> --edit`: the notes panel under a browser pane that is
//! in annotate mode (spec 06 B11). It lists the numbered notes of the pane's draft (element,
//! role and name, page text, the saved screenshot), and lets you edit a note, delete one, add a
//! page note, send the draft to the agent, copy it as text (OSC 52) or clear it. This is what the
//! notes split runs (`browser.pane.annotations`). Without `--edit` the verb stays the plain
//! `browser.annotations` call.
//!
//! The panel turns on bracketed paste: a paste is text for the note being edited (one line),
//! and is ignored outside the editor, so pasted letters never act as shortcuts.
//!
//! The panel state ([`Panel`]) and the key parser ([`KeyParser`]) are pure; [`run`] owns the
//! terminal and the API calls. Everything the page supplied (selectors, text, names) is escaped
//! before it is drawn ([`vk_proto::text::escape_controls`]).

use crate::client::{CallError, Client};
use crate::show::{self, Mode as GfxMode};
use crate::{EXIT_API, EXIT_OK, EXIT_USAGE, exit_code_for, print_error};
use base64::Engine as _;
use serde_json::{Value, json};
use std::io::{IsTerminal, Write};
use std::time::Duration;
use vk_proto::text::escape_controls;

/// Poll interval for changes made elsewhere (picks, the web and desktop notes panels).
const POLL: Duration = Duration::from_millis(700);
/// How long a lone ESC waits for the rest of an escape sequence.
const ESC_WAIT: Duration = Duration::from_millis(40);
/// Rows reserved under the panel for a thumbnail, and its width in cells.
const THUMB_ROWS: u16 = 5;
const THUMB_COLS: u16 = 12;
/// Largest screenshot read for the thumbnail.
const THUMB_MAX: u64 = 2 << 20;
/// Rows each note takes in the list.
const NOTE_ROWS: usize = 3;
/// Most bytes of one bracketed paste kept (the rest is dropped).
const PASTE_MAX: usize = 64 << 10;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// One note of a draft (only the fields the panel shows).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Note {
    pub n: u32,
    pub kind: String,
    pub selector: String,
    pub role: String,
    pub name: String,
    pub text: String,
    pub image: Option<String>,
    pub note: String,
}

/// The draft the panel edits.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Draft {
    pub id: String,
    pub url: String,
    pub state: String,
    pub notes: Vec<Note>,
    pub page_note: String,
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

impl Draft {
    /// Parse an `AnnotationDraft`; `None` for null or anything without an id.
    pub fn parse(v: &Value) -> Option<Draft> {
        let id = v.get("id")?.as_str()?.to_string();
        let notes = v["notes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|n| Note {
                        n: n["n"].as_u64().unwrap_or(0) as u32,
                        kind: s(n, "kind"),
                        selector: s(n, "selector"),
                        role: s(n, "role"),
                        name: s(n, "name"),
                        text: s(n, "text"),
                        image: n.get("image").and_then(Value::as_str).map(String::from),
                        note: s(n, "note"),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(Draft {
            id,
            url: s(v, "url"),
            state: s(v, "state"),
            notes,
            page_note: s(v, "page_note"),
        })
    }
}

// ---- keys -------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Esc,
    Backspace,
    Ctrl(char),
    Char(char),
    /// A bracketed paste (`ESC [200~ … ESC [201~`): text, never commands.
    Paste(String),
    Other,
}

/// Turns terminal bytes into keys: arrows (`ESC [ A`), UTF-8 characters, control keys, and
/// bracketed pastes as one [`Key::Paste`].
#[derive(Default)]
pub struct KeyParser {
    esc: Vec<u8>,
    utf8: Vec<u8>,
    /// Inside a bracketed paste: the bytes so far (at most [`PASTE_MAX`] plus the end marker).
    paste: Option<Vec<u8>>,
}

impl KeyParser {
    pub fn feed(&mut self, b: u8) -> Option<Key> {
        if let Some(buf) = &mut self.paste {
            buf.push(b);
            if buf.ends_with(PASTE_END) {
                let mut buf = self.paste.take().unwrap_or_default();
                buf.truncate((buf.len() - PASTE_END.len()).min(PASTE_MAX));
                return Some(Key::Paste(String::from_utf8_lossy(&buf).into_owned()));
            }
            // Keep the first `PASTE_MAX` bytes, and the last few to find the end marker.
            if buf.len() > PASTE_MAX + PASTE_END.len() {
                buf.remove(PASTE_MAX);
            }
            return None;
        }
        if !self.esc.is_empty() {
            self.esc.push(b);
            if self.esc.len() == 2 {
                if b == b'[' || b == b'O' {
                    return None;
                }
                self.esc.clear();
                return Some(Key::Other);
            }
            if (0x40..=0x7e).contains(&b) {
                if self.esc == PASTE_START {
                    self.esc.clear();
                    self.paste = Some(Vec::new());
                    return None;
                }
                self.esc.clear();
                return Some(match b {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    _ => Key::Other,
                });
            }
            if self.esc.len() > 16 {
                self.esc.clear();
                return Some(Key::Other);
            }
            return None;
        }
        if !self.utf8.is_empty() {
            self.utf8.push(b);
            let need = match self.utf8[0] {
                0xf0..=0xf7 => 4,
                0xe0..=0xef => 3,
                _ => 2,
            };
            if self.utf8.len() < need {
                return None;
            }
            let bytes = std::mem::take(&mut self.utf8);
            return Some(
                std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|t| t.chars().next())
                    .map_or(Key::Other, Key::Char),
            );
        }
        Some(match b {
            0x1b => {
                self.esc.push(b);
                return None;
            }
            b'\r' | b'\n' => Key::Enter,
            0x7f | 0x08 => Key::Backspace,
            1..=26 => Key::Ctrl((b'a' + b - 1) as char),
            0x20..=0x7e => Key::Char(b as char),
            0xc0..=0xf7 => {
                self.utf8.push(b);
                return None;
            }
            _ => Key::Other,
        })
    }

    /// No more bytes are coming for now: a pending lone ESC is the Esc key. A paste in progress
    /// waits for its end marker.
    pub fn flush(&mut self) -> Option<Key> {
        if self.paste.is_some() {
            return None;
        }
        if self.esc.len() == 1 {
            self.esc.clear();
            return Some(Key::Esc);
        }
        self.esc.clear();
        self.utf8.clear();
        None
    }
}

// ---- panel ------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Note(u32),
    Page,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    List,
    Edit { target: Target, buf: String },
    ConfirmClear,
}

/// What the loop does after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    SaveNote { n: u32, note: String },
    SavePage(String),
    Remove(u32),
    Send,
    Copy,
    Clear,
}

pub struct Panel {
    pub pane: String,
    pub draft: Option<Draft>,
    pub sel: usize,
    pub status: String,
    mode: Mode,
}

impl Panel {
    pub fn new(pane: &str) -> Panel {
        Panel {
            pane: pane.to_string(),
            draft: None,
            sel: 0,
            status: String::new(),
            mode: Mode::List,
        }
    }

    /// Replace the draft (after a poll or a reply); keeps the selection on the same note number
    /// when it still exists.
    pub fn set_draft(&mut self, d: Option<Draft>) {
        let keep = self.selected().map(|n| n.n);
        self.draft = d;
        let len = self.notes().len();
        self.sel = keep
            .and_then(|n| self.notes().iter().position(|x| x.n == n))
            .unwrap_or(self.sel)
            .min(len.saturating_sub(1));
    }

    fn notes(&self) -> &[Note] {
        self.draft.as_ref().map_or(&[][..], |d| d.notes.as_slice())
    }

    pub fn selected(&self) -> Option<&Note> {
        self.notes().get(self.sel)
    }

    pub fn editing(&self) -> bool {
        matches!(self.mode, Mode::Edit { .. })
    }

    pub fn key(&mut self, k: Key) -> Action {
        match &mut self.mode {
            Mode::Edit { target, buf } => match k {
                Key::Enter => {
                    let text = std::mem::take(buf);
                    let t = target.clone();
                    self.mode = Mode::List;
                    match t {
                        Target::Note(n) => Action::SaveNote { n, note: text },
                        Target::Page => Action::SavePage(text),
                    }
                }
                Key::Esc | Key::Ctrl('c') => {
                    self.mode = Mode::List;
                    Action::None
                }
                Key::Backspace => {
                    buf.pop();
                    Action::None
                }
                Key::Ctrl('u') => {
                    buf.clear();
                    Action::None
                }
                Key::Char(c) => {
                    buf.push(c);
                    Action::None
                }
                Key::Paste(t) => {
                    buf.push_str(&paste_text(&t));
                    Action::None
                }
                _ => Action::None,
            },
            Mode::ConfirmClear => {
                let yes = k == Key::Char('y');
                self.mode = Mode::List;
                self.status.clear();
                if yes { Action::Clear } else { Action::None }
            }
            Mode::List => self.list_key(k),
        }
    }

    fn list_key(&mut self, k: Key) -> Action {
        let len = self.notes().len();
        match k {
            // Pasted text is never a command.
            Key::Paste(_) => {
                self.status = "paste ignored: press enter to edit a note first".into();
            }
            Key::Up | Key::Char('k') => self.sel = self.sel.saturating_sub(1),
            Key::Down | Key::Char('j') => self.sel = (self.sel + 1).min(len.saturating_sub(1)),
            Key::Enter | Key::Char('e') => {
                if let Some(n) = self.selected() {
                    self.mode = Mode::Edit {
                        target: Target::Note(n.n),
                        buf: n.note.clone(),
                    };
                    self.status.clear();
                } else {
                    self.status = "no notes yet: click an element in the page".into();
                }
            }
            Key::Char('p') => {
                if self.draft.is_none() {
                    self.status = "no draft yet: click an element in the page first".into();
                } else {
                    let buf = self
                        .draft
                        .as_ref()
                        .map_or(String::new(), |d| d.page_note.clone());
                    self.mode = Mode::Edit {
                        target: Target::Page,
                        buf,
                    };
                    self.status.clear();
                }
            }
            Key::Char('d') => {
                if let Some(n) = self.selected() {
                    return Action::Remove(n.n);
                }
            }
            Key::Char('s') | Key::Char('c') => {
                let has = self
                    .draft
                    .as_ref()
                    .is_some_and(|d| !d.notes.is_empty() || !d.page_note.is_empty());
                if !has {
                    self.status = "nothing to send yet".into();
                    return Action::None;
                }
                return if k == Key::Char('s') {
                    Action::Send
                } else {
                    Action::Copy
                };
            }
            Key::Char('x') => {
                if self.draft.is_some() {
                    self.mode = Mode::ConfirmClear;
                    self.status = "clear all notes? y to confirm".into();
                }
            }
            Key::Char('q') | Key::Esc | Key::Ctrl('c') | Key::Ctrl('d') => return Action::Quit,
            _ => {}
        }
        Action::None
    }

    /// `height` lines of at most `cols` cells: header, optional page note, the notes around the
    /// selection (three lines each), then a status line and the key help.
    pub fn render(&self, height: usize, cols: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let (count, state) = self
            .draft
            .as_ref()
            .map_or((0, ""), |d| (d.notes.len(), d.state.as_str()));
        let plural = if count == 1 { "note" } else { "notes" };
        let mut head = format!(" annotations · {} · {count} {plural}", clean(&self.pane));
        if !state.is_empty() {
            head.push_str(&format!(" · {}", clean(state)));
        }
        out.push(format!("\x1b[7m{:<w$}\x1b[0m", fit(&head, cols), w = cols));
        if let Some(d) = &self.draft {
            if !d.page_note.is_empty() {
                out.push(fit(&format!(" page note: {}", clean(&d.page_note)), cols));
            }
            if !d.url.is_empty() {
                out.push(format!(
                    "\x1b[2m{}\x1b[0m",
                    fit(&format!(" {}", clean(&d.url)), cols)
                ));
            }
        }
        let footer = 2;
        let room = height.saturating_sub(out.len() + footer);
        let notes = self.notes();
        if notes.is_empty() {
            out.push(" no notes yet — click an element in the page, or shift-drag a region".into());
        } else {
            let per = (room / NOTE_ROWS).max(1);
            let first = if self.sel >= per {
                self.sel + 1 - per
            } else {
                0
            };
            for (i, n) in notes.iter().enumerate().skip(first).take(per) {
                let cur = i == self.sel;
                let mark = if cur { "▸" } else { " " };
                let who = match (n.role.as_str(), n.name.as_str()) {
                    ("", "") => n.kind.clone(),
                    (r, "") => r.to_string(),
                    ("", nm) => format!("\"{nm}\""),
                    (r, nm) => format!("{r} \"{nm}\""),
                };
                let l1 = format!("{mark}{:>2} {}", n.n, clean(&who));
                let detail = if n.selector.is_empty() {
                    clean(&n.text)
                } else if n.text.is_empty() {
                    clean(&n.selector)
                } else {
                    format!("{} · {}", clean(&n.selector), clean(&n.text))
                };
                let img = n
                    .image
                    .as_deref()
                    .map(|p| format!(" [{}]", clean(p)))
                    .unwrap_or_default();
                let l3 = match &self.mode {
                    Mode::Edit {
                        target: Target::Note(m),
                        buf,
                    } if *m == n.n => format!("    note: {}█", clean(buf)),
                    _ if n.note.is_empty() => "    note: (none — enter to add)".to_string(),
                    _ => format!("    note: {}", clean(&n.note)),
                };
                let l1 = fit(&l1, cols);
                out.push(if cur {
                    format!("\x1b[1m{l1}\x1b[0m")
                } else {
                    l1
                });
                out.push(format!(
                    "\x1b[2m{}\x1b[0m",
                    fit(&format!("    {detail}{img}"), cols)
                ));
                out.push(fit(&l3, cols));
            }
        }
        while out.len() + footer < height {
            out.push(String::new());
        }
        out.truncate(height.saturating_sub(footer));
        let status = match &self.mode {
            Mode::Edit {
                target: Target::Page,
                buf,
            } => format!(" page note: {}█", clean(buf)),
            _ => format!(" {}", clean(&self.status)),
        };
        out.push(fit(&status, cols));
        let help = match &self.mode {
            Mode::Edit { .. } => " enter save · esc cancel · ctrl-u clear",
            Mode::ConfirmClear => " y clear everything · any other key cancels",
            Mode::List => {
                " ↑↓ select · enter edit · p page note · d delete · s send · c copy · x clear · q quit"
            }
        };
        out.push(format!("\x1b[7m{:<w$}\x1b[0m", fit(help, cols), w = cols));
        out
    }
}

/// Pasted text for the one-line editor: line breaks and tabs become spaces, other control
/// characters are dropped.
fn paste_text(t: &str) -> String {
    let t = t.replace("\r\n", "\n");
    t.chars()
        .filter_map(|c| match c {
            '\n' | '\r' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// Page-supplied text made safe for the terminal: control characters escaped, newlines flattened.
fn clean(t: &str) -> String {
    escape_controls(t).into_owned()
}

/// Cut `t` to `cols` characters (an ellipsis marks the cut). Escape sequences this module adds
/// itself are applied after fitting, never inside it.
fn fit(t: &str, cols: usize) -> String {
    if t.chars().count() <= cols {
        return t.to_string();
    }
    let mut o: String = t.chars().take(cols.saturating_sub(1)).collect();
    o.push('…');
    o
}

/// The OSC 52 sequence that puts `text` on the terminal's clipboard.
pub fn osc52(text: &str) -> String {
    format!(
        "\x1b]52;c;{}\x1b\\",
        base64::engine::general_purpose::STANDARD.encode(text)
    )
}

// ---- terminal ---------------------------------------------------------------------------------

struct RawStdin(Option<libc::termios>);

impl RawStdin {
    fn enter() -> RawStdin {
        // SAFETY: tcgetattr/tcsetattr on fd 0 with a termios we own.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return RawStdin(None);
            }
            let saved = t;
            t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            t.c_cc[libc::VMIN] = 1;
            t.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return RawStdin(None);
            }
            RawStdin(Some(saved))
        }
    }
}

impl Drop for RawStdin {
    fn drop(&mut self) {
        if let Some(t) = self.0 {
            // SAFETY: restoring the attributes read in `enter`.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &t);
            }
        }
    }
}

/// The alternate screen with the cursor hidden; left again on drop.
struct Screen;

impl Screen {
    fn enter() -> Screen {
        // Bracketed paste: pasted text arrives marked, so it is never read as commands.
        print!("\x1b[?1049h\x1b[?25l\x1b[?2004h");
        let _ = std::io::stdout().flush();
        Screen
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        print!("\x1b_Ga=d,q=2\x1b\\\x1b[?2004l\x1b[?25h\x1b[?1049l");
        let _ = std::io::stdout().flush();
    }
}

fn terminal_size() -> (usize, usize) {
    // SAFETY: TIOCGWINSZ fills a plain struct on a valid fd; failure leaves it zeroed.
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            return (ws.ws_col as usize, ws.ws_row as usize);
        }
    }
    (80, 24)
}

/// The screenshot of the selected note for the thumbnail (a regular file up to [`THUMB_MAX`]).
fn thumbnail(panel: &Panel, mode: GfxMode) -> Option<Vec<u8>> {
    if mode == GfxMode::Text {
        return None;
    }
    let path = panel.selected()?.image.as_deref()?;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > THUMB_MAX {
        return None;
    }
    let png = std::fs::read(path).ok()?;
    let mut bytes = show::image_bytes(mode, &png, "note.png", THUMB_COLS);
    while bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    Some(bytes)
}

fn draw(panel: &Panel, mode: GfxMode, last: &mut String) {
    let (cols, rows) = terminal_size();
    let want_thumb =
        mode != GfxMode::Text && rows >= 20 && panel.selected().is_some_and(|n| n.image.is_some());
    let body = rows.saturating_sub(if want_thumb { THUMB_ROWS as usize } else { 0 });
    let lines = panel.render(body, cols);
    let mut frame = String::from("\x1b_Ga=d,q=2\x1b\\\x1b[H");
    for (i, l) in lines.iter().enumerate() {
        frame.push_str(&format!("\x1b[{};1H\x1b[2K{l}", i + 1));
    }
    let mut bytes = frame.into_bytes();
    if want_thumb && let Some(img) = thumbnail(panel, mode) {
        bytes.extend_from_slice(format!("\x1b[{};1H", body + 1).as_bytes());
        bytes.extend_from_slice(&img);
    }
    // Only repaint on a change: a repeated image upload every poll would flicker.
    let sig = String::from_utf8_lossy(&bytes).into_owned();
    if *last == sig {
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(b"\x1b[2J");
    let _ = out.write_all(&bytes);
    let _ = out.flush();
    *last = sig;
}

// ---- the command ------------------------------------------------------------------------------

async fn fetch<S>(client: &mut Client<S>, pane: &str) -> Result<Option<Draft>, CallError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let v = client
        .call("browser.annotations", json!({"pane": pane}))
        .await?;
    Ok(Draft::parse(&v["draft"]))
}

/// What a send reply tells the user.
pub fn describe_send(v: &Value) -> String {
    let agent = v["agent_pane"].as_str().map(clean);
    match (v["state"].as_str(), v["via"].as_str()) {
        (Some("delivered"), Some(via)) => match agent {
            Some(a) => format!("sent to {a} ({})", clean(via)),
            None => format!("sent ({})", clean(via)),
        },
        (Some("delivered"), None) => "sent".into(),
        (Some("sending"), _) => match agent {
            Some(a) => format!("sending to {a}"),
            None => "sending".into(),
        },
        (Some("unconfirmed"), _) => {
            "not confirmed: check the agent and send again if it did not arrive".into()
        }
        (Some("queued"), _) => "queued: the agent gets it when it is ready".into(),
        _ => "sent".into(),
    }
}

/// Run `browser annotations --pane P --edit`. Returns the exit code.
pub async fn run<S>(client: &mut Client<S>, g: &crate::Global, p: Value) -> i32
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let Some(pane) = p.get("pane").map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }) else {
        eprintln!("vibeke browser annotations --pane <browser pane> --edit");
        return EXIT_USAGE;
    };
    if let Err(e) = client.hello("cli").await {
        print_error(&e);
        return exit_code_for(&e);
    }
    let interactive = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    if !interactive || g.json == Some(true) {
        // No terminal for a panel: the plain read.
        return match client
            .call("browser.annotations", json!({"pane": pane}))
            .await
        {
            Ok(v) => {
                println!("{}", serde_json::to_string(&v).unwrap_or_default());
                EXIT_OK
            }
            Err(e) => {
                print_error(&e);
                exit_code_for(&e)
            }
        };
    }
    let env = |k: &str| std::env::var(k).ok();
    let gfx = show::detect(&env, true);
    let _raw = RawStdin::enter();
    let _screen = Screen::enter();
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<u8>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut b = [0u8; 1];
        while let Ok(1) = std::io::stdin().read(&mut b) {
            if key_tx.send(b[0]).is_err() {
                break;
            }
        }
    });
    let mut panel = Panel::new(&pane);
    let mut parser = KeyParser::default();
    let mut last = String::new();
    loop {
        match fetch(client, &pane).await {
            Ok(d) => panel.set_draft(d),
            Err(CallError::Rpc(r)) if r.data.kind == "not_found" => {
                drop(_screen);
                println!("browser pane {pane} is gone");
                return EXIT_OK;
            }
            Err(e) => {
                drop(_screen);
                print_error(&e);
                return if matches!(e, CallError::Io(_)) {
                    EXIT_API
                } else {
                    exit_code_for(&e)
                };
            }
        }
        draw(&panel, gfx, &mut last);
        // Wait for a key (or the next poll); a lone ESC is flushed after a short pause.
        let mut keys = Vec::new();
        let first = tokio::time::timeout(POLL, key_rx.recv()).await;
        match first {
            Ok(Some(b)) => {
                keys.extend(parser.feed(b));
                loop {
                    match tokio::time::timeout(ESC_WAIT, key_rx.recv()).await {
                        Ok(Some(b)) => keys.extend(parser.feed(b)),
                        Ok(None) => return EXIT_OK,
                        Err(_) => {
                            keys.extend(parser.flush());
                            break;
                        }
                    }
                }
            }
            Ok(None) => return EXIT_OK,
            Err(_) => {}
        }
        for k in keys {
            let action = panel.key(k);
            if action == Action::Quit {
                return EXIT_OK;
            }
            apply(client, &mut panel, action).await;
        }
        draw(&panel, gfx, &mut last);
    }
}

/// Run the API call an action asks for and fold the reply into the panel.
async fn apply<S>(client: &mut Client<S>, panel: &mut Panel, action: Action)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let Some(id) = panel.draft.as_ref().map(|d| d.id.clone()) else {
        return;
    };
    let call = match &action {
        Action::None | Action::Quit => return,
        Action::SaveNote { n, note } => (
            "browser.annotation_update",
            json!({"draft": id, "n": n, "note": note}),
        ),
        Action::SavePage(t) => (
            "browser.annotation_update",
            json!({"draft": id, "page_note": t}),
        ),
        Action::Remove(n) => ("browser.annotation_remove", json!({"draft": id, "n": n})),
        Action::Send => ("browser.annotations_send", json!({"draft": id})),
        Action::Copy => (
            "browser.annotations_send",
            json!({"draft": id, "copy_only": true}),
        ),
        Action::Clear => ("browser.annotations_clear", json!({"draft": id})),
    };
    match client.call(call.0, call.1).await {
        Ok(v) => {
            match action {
                Action::Send => panel.status = describe_send(&v),
                Action::Copy => {
                    let text = v["text"].as_str().unwrap_or("");
                    print!("{}", osc52(text));
                    let _ = std::io::stdout().flush();
                    panel.status = format!("copied {} characters", text.chars().count());
                }
                Action::Clear => panel.status = "cleared".into(),
                Action::Remove(_) => panel.status = "deleted".into(),
                _ => panel.status.clear(),
            }
            if let Some(d) = Draft::parse(&v["draft"]) {
                panel.set_draft(Some(d));
            }
        }
        Err(e) => panel.status = format!("failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> Draft {
        Draft::parse(&json!({
            "id": "d1", "url": "http://localhost:5173/", "state": "open", "page_note": "",
            "notes": [
                {"n": 1, "kind": "element", "selector": "button.save", "role": "button",
                 "name": "Save", "text": "Save changes", "note": "too small",
                 "image": "/tmp/a.png"},
                {"n": 2, "kind": "region", "selector": "div.card", "note": ""},
                {"n": 3, "kind": "element", "selector": "h1", "text": "Title", "note": "typo"}
            ]
        }))
        .unwrap()
    }

    fn panel() -> Panel {
        let mut p = Panel::new("bp");
        p.set_draft(Some(draft()));
        p
    }

    fn keys(p: &mut Panel, s: &str) -> Vec<Action> {
        s.chars().map(|c| p.key(Key::Char(c))).collect()
    }

    #[test]
    fn parses_a_draft() {
        let d = draft();
        assert_eq!(d.notes.len(), 3);
        assert_eq!(d.notes[0].image.as_deref(), Some("/tmp/a.png"));
        assert_eq!(d.notes[1].role, "");
        assert!(Draft::parse(&Value::Null).is_none());
    }

    #[test]
    fn parser_reads_arrows_utf8_and_a_lone_esc() {
        let mut kp = KeyParser::default();
        let mut got = Vec::new();
        for b in b"\x1b[A\x1b[Bq\r\x7f\x03" {
            got.extend(kp.feed(*b));
        }
        assert_eq!(
            got,
            vec![
                Key::Up,
                Key::Down,
                Key::Char('q'),
                Key::Enter,
                Key::Backspace,
                Key::Ctrl('c')
            ]
        );
        let mut got = Vec::new();
        for b in "æ".as_bytes() {
            got.extend(kp.feed(*b));
        }
        assert_eq!(got, vec![Key::Char('æ')]);
        assert_eq!(kp.feed(0x1b), None);
        assert_eq!(kp.flush(), Some(Key::Esc));
        assert_eq!(kp.flush(), None);
        // An unknown sequence is swallowed whole.
        let mut got = Vec::new();
        for b in b"\x1b[1;5Cx" {
            got.extend(kp.feed(*b));
        }
        assert_eq!(got, vec![Key::Other, Key::Char('x')]);
    }

    #[test]
    fn parser_reads_a_bracketed_paste_as_one_key() {
        let mut kp = KeyParser::default();
        let mut got = Vec::new();
        for b in b"a\x1b[200~sxq\r\n\x1b[Ad\x1b[201~b" {
            got.extend(kp.feed(*b));
        }
        assert_eq!(
            got,
            vec![
                Key::Char('a'),
                Key::Paste("sxq\r\n\x1b[Ad".into()),
                Key::Char('b')
            ]
        );
        // A pause in the middle of a paste does not end it.
        let mut got = Vec::new();
        for b in b"\x1b[200~he" {
            got.extend(kp.feed(*b));
        }
        assert_eq!(kp.flush(), None);
        for b in "llo æ\x1b[201~".as_bytes() {
            got.extend(kp.feed(*b));
        }
        assert_eq!(got, vec![Key::Paste("hello æ".into())]);
        // A huge paste is cut, and its end is still found.
        let mut got = Vec::new();
        for b in PASTE_START {
            got.extend(kp.feed(*b));
        }
        for _ in 0..PASTE_MAX + 100 {
            got.extend(kp.feed(b'x'));
        }
        for b in PASTE_END.iter().chain(b"q") {
            got.extend(kp.feed(*b));
        }
        match &got[..] {
            [Key::Paste(t), Key::Char('q')] => assert_eq!(t.len(), PASTE_MAX),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_paste_is_editor_text_and_never_a_command() {
        let mut p = panel();
        // Outside the editor: ignored, whatever letters it holds.
        assert_eq!(p.key(Key::Paste("sxyq".into())), Action::None);
        assert!(p.status.contains("paste ignored"), "{}", p.status);
        assert!(!p.editing());
        p.key(Key::Char('x'));
        assert_eq!(p.key(Key::Paste("y".into())), Action::None, "no clear");
        // In the editor: one line of text; Enter in the paste does not save.
        p.key(Key::Char('e'));
        p.key(Key::Ctrl('u'));
        assert_eq!(
            p.key(Key::Paste("make it\r\nbigger\tand \x1b[2Jblue\n".into())),
            Action::None
        );
        assert!(p.editing());
        assert_eq!(
            p.key(Key::Enter),
            Action::SaveNote {
                n: 1,
                note: "make it bigger and [2Jblue ".into()
            }
        );
    }

    #[test]
    fn selection_moves_and_clamps() {
        let mut p = panel();
        assert_eq!(p.key(Key::Up), Action::None);
        assert_eq!(p.sel, 0);
        p.key(Key::Down);
        p.key(Key::Char('j'));
        p.key(Key::Down);
        assert_eq!(p.sel, 2);
        // The draft shrinks under the selection.
        let mut d = draft();
        d.notes.truncate(1);
        p.set_draft(Some(d));
        assert_eq!(p.sel, 0);
    }

    #[test]
    fn selection_follows_the_note_number_across_refreshes() {
        let mut p = panel();
        p.key(Key::Down);
        p.key(Key::Down);
        assert_eq!(p.selected().unwrap().n, 3);
        let mut d = draft();
        d.notes.remove(0);
        p.set_draft(Some(d));
        assert_eq!(p.selected().unwrap().n, 3);
    }

    #[test]
    fn edit_saves_on_enter_and_cancels_on_esc() {
        let mut p = panel();
        assert_eq!(p.key(Key::Enter), Action::None);
        assert!(p.editing());
        // The buffer starts with the current note text.
        for _ in 0.."too small".len() {
            p.key(Key::Backspace);
        }
        keys(&mut p, "bigger");
        assert!(p.render(20, 80).iter().any(|l| l.contains("note: bigger█")));
        assert_eq!(
            p.key(Key::Enter),
            Action::SaveNote {
                n: 1,
                note: "bigger".into()
            }
        );
        assert!(!p.editing());
        p.key(Key::Char('e'));
        keys(&mut p, "x");
        assert_eq!(p.key(Key::Esc), Action::None);
        assert!(!p.editing());
        // `q` inside the editor is text, not quit.
        p.key(Key::Char('e'));
        assert_eq!(p.key(Key::Char('q')), Action::None);
        assert!(p.editing());
    }

    #[test]
    fn page_note_delete_send_copy_clear_quit() {
        let mut p = panel();
        p.key(Key::Char('p'));
        keys(&mut p, "overall");
        assert_eq!(p.key(Key::Enter), Action::SavePage("overall".into()));
        p.key(Key::Down);
        assert_eq!(p.key(Key::Char('d')), Action::Remove(2));
        assert_eq!(p.key(Key::Char('s')), Action::Send);
        assert_eq!(p.key(Key::Char('c')), Action::Copy);
        // Clear asks first; anything but `y` cancels.
        assert_eq!(p.key(Key::Char('x')), Action::None);
        assert!(p.render(20, 80).iter().any(|l| l.contains("y clear")));
        assert_eq!(p.key(Key::Char('n')), Action::None);
        p.key(Key::Char('x'));
        assert_eq!(p.key(Key::Char('y')), Action::Clear);
        assert_eq!(p.key(Key::Char('q')), Action::Quit);
        assert_eq!(p.key(Key::Esc), Action::Quit);
    }

    #[test]
    fn empty_panel_has_nothing_to_send_or_edit() {
        let mut p = Panel::new("bp");
        assert_eq!(p.key(Key::Char('s')), Action::None);
        assert!(p.status.contains("nothing"));
        assert_eq!(p.key(Key::Enter), Action::None);
        assert!(!p.editing());
        assert!(p.render(10, 60).iter().any(|l| l.contains("no notes yet")));
    }

    #[test]
    fn render_lists_numbered_notes_with_the_selection_marked() {
        let mut p = panel();
        let out = p.render(24, 80).join("\n");
        assert!(out.contains("3 notes"), "{out}");
        assert!(out.contains("▸ 1 button \"Save\""), "{out}");
        assert!(out.contains("button.save · Save changes"), "{out}");
        assert!(out.contains("[/tmp/a.png]"), "{out}");
        assert!(out.contains("note: too small"), "{out}");
        assert!(
            out.contains("  3 element") || out.contains(" 3 element"),
            "{out}"
        );
        p.key(Key::Down);
        assert!(p.render(24, 80).join("\n").contains("▸ 2 region"));
        assert_eq!(p.render(24, 80).len(), 24);
        assert_eq!(p.render(10, 40).len(), 10);
    }

    #[test]
    fn render_windows_the_list_around_the_selection() {
        let mut d = draft();
        for n in 4..=9 {
            d.notes.push(Note {
                n,
                kind: "element".into(),
                selector: format!("li:nth-child({n})"),
                ..Default::default()
            });
        }
        let mut p = Panel::new("bp");
        p.set_draft(Some(d));
        for _ in 0..8 {
            p.key(Key::Down);
        }
        // Room for two notes: header + url + 2*3 + 2 footer rows.
        let out = p.render(10, 80).join("\n");
        assert!(out.contains("▸ 9"), "{out}");
        assert!(!out.contains(" 1 button"), "{out}");
    }

    #[test]
    fn page_text_cannot_reach_the_terminal_as_control_sequences() {
        let hostile = "x\x1b]52;c;YXR0YWNrZXI=\x07\x1b[2J\u{9b}0m";
        let mut d = draft();
        d.notes[0].name = hostile.into();
        d.notes[0].selector = hostile.into();
        d.notes[0].text = hostile.into();
        d.notes[0].note = hostile.into();
        d.page_note = hostile.into();
        d.url = hostile.into();
        let mut p = Panel::new(hostile);
        p.set_draft(Some(d));
        let out = p.render(24, 120).join("\n");
        assert!(!out.contains("\x1b]52"), "{out:?}");
        assert!(!out.contains("\x1b[2J"), "{out:?}");
        assert!(!out.contains('\u{9b}'), "{out:?}");
        assert!(!out.contains('\x07'), "{out:?}");
        // The only escapes left are the SGR styles the panel adds itself.
        let mut rest = out.as_str();
        while let Some(i) = rest.find('\x1b') {
            let tail = &rest[i..];
            assert!(
                tail.starts_with("\x1b[7m")
                    || tail.starts_with("\x1b[0m")
                    || tail.starts_with("\x1b[1m")
                    || tail.starts_with("\x1b[2m"),
                "{tail:?}"
            );
            rest = &tail[1..];
        }
    }

    #[test]
    fn lines_are_cut_to_the_width() {
        let mut d = draft();
        d.notes[0].note = "w".repeat(300);
        let mut p = Panel::new("bp");
        p.set_draft(Some(d));
        for l in p.render(24, 40) {
            let visible: String = {
                let mut v = String::new();
                let mut esc = false;
                for c in l.chars() {
                    match (esc, c) {
                        (false, '\x1b') => esc = true,
                        (true, 'm') => esc = false,
                        (false, c) => v.push(c),
                        _ => {}
                    }
                }
                v
            };
            assert!(visible.chars().count() <= 40, "{visible:?}");
        }
    }

    #[test]
    fn osc52_and_send_wording() {
        assert_eq!(osc52("hello"), "\x1b]52;c;aGVsbG8=\x1b\\");
        assert_eq!(
            describe_send(&json!({"state": "delivered", "via": "typed", "agent_pane": "p3"})),
            "sent to p3 (typed)"
        );
        assert!(describe_send(&json!({"state": "queued"})).starts_with("queued"));
        assert_eq!(
            describe_send(&json!({"state": "sending", "via": "typed", "agent_pane": "p3"})),
            "sending to p3"
        );
        assert!(describe_send(&json!({"state": "unconfirmed"})).starts_with("not confirmed"));
    }
}
