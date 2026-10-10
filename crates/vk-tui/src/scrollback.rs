//! Edit scrollback (03 §11.3, 08 §13; `edit_scrollback`, `prefix+e`): the focused pane's whole
//! history — archive segments, in-memory scrollback and the screen — loaded with `pane.read
//! {source: archive}` (pages of 5000 rows, newest first, up to [`MAX_ROWS`]), soft wraps joined
//! into logical lines, shown in a read-only viewer positioned at the current viewport (the live
//! screen, or copy mode's view when opened from there).
//!
//! The viewer scrolls (`j k`, `space`/`PgDn`, `PgUp`, `g G`), searches (`/`, `n`/`N`, smart case)
//! and `e` opens the same text in `$VISUAL`/`$EDITOR`: it is written to a private temp file
//! (directory 0700, file made read-only 0400) and the TUI suspends while the editor runs on the
//! host terminal, then the file is deleted. Remote panes work the same way: the text is
//! fetched to this client. Nothing is sent to the pane.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::drafts::Area;
use crate::screen::Grid;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

/// Rows per `pane.read` page (the server's cap).
pub const PAGE: u64 = 5000;
/// Most rows loaded into the viewer / editor file.
pub const MAX_ROWS: usize = 200_000;

#[derive(Debug, Clone)]
pub enum Reply {
    Page { seq: u64 },
}

/// An external program to run with the TUI suspended (the main loop owns the terminal).
#[derive(Debug, PartialEq)]
pub struct External {
    pub argv: Vec<String>,
    /// Deleted after the program exits ([`run_external`]), or when this is dropped without
    /// running (the app quit or unwound first).
    pub file: TempFile,
}

/// A temp file that is removed when dropped unless [`TempFile::persist`]ed: every failure,
/// early return or unwind between creating the file and its consumer finishing cleans up.
#[derive(Debug, PartialEq)]
pub struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl TempFile {
    #[cfg(not(target_arch = "wasm32"))]
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Disarm: the caller now owns deleting the file.
    pub fn persist(mut self) -> PathBuf {
        self.armed = false;
        std::mem::take(&mut self.path)
    }
}

impl std::ops::Deref for TempFile {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempFile {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            remove_file(&self.path);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScrollbackView {
    pub machine: usize,
    pub pane: String,
    pub title: String,
    /// Raw rows `(absolute line, text, wrapped)`, oldest first.
    rows: Vec<(u64, String, bool)>,
    /// Logical lines `(absolute line of the first row, text)`.
    pub lines: Vec<(u64, String)>,
    pub loading: bool,
    pub seq: u64,
    /// Older rows exist that were not loaded (the cap was reached).
    pub truncated: bool,
    /// First shown logical line.
    pub top: usize,
    /// Absolute line to show at the top once loaded.
    pub goal: Option<u64>,
    /// Search input being typed.
    pub search: Option<String>,
    pub last_search: Option<String>,
    pub message: Option<String>,
    pub error: Option<String>,
    /// Body rows at the last draw (paging).
    pub page: usize,
    /// Where editor temp files go (a private per-user directory under the system temp dir).
    pub dir: PathBuf,
    /// Mouse selection `(anchor, end)` as (logical line, char index), inclusive.
    pub sel: Option<((usize, usize), (usize, usize))>,
    /// The left button is down over the text (a drag extends `sel`).
    pub selecting: bool,
    /// First in-memory line of the pane (from the archive reply), for styled rows.
    pub mem_first: Option<u64>,
    /// `copy_mode.editor_include_ansi`: the styled-rows fetch before the editor opens.
    pub ansi: Option<AnsiFetch>,
}

/// Styled in-memory rows for the editor file (`FetchHistory`: the size first, then the rows).
#[derive(Debug, Clone, PartialEq)]
pub struct AnsiFetch {
    pub req: u64,
    /// The size was asked; the rows come next.
    pub rows_asked: bool,
    pub argv: Vec<String>,
}

/// Most in-memory rows fetched with colours.
pub const ANSI_ROWS: u32 = 20_000;

/// SGR for one row's spans (`trim_end`: drop trailing blanks with a default background, as the
/// text form trims the line), reset at the end when anything was styled. Control characters in
/// cell text are dropped so the file never carries other escapes.
pub fn row_ansi(row: &vk_proto::render::Row, trim_end: bool) -> String {
    use vk_proto::render::{Color, Style, attr};
    let mut spans: Vec<(Style, String)> = row
        .spans
        .iter()
        .map(|s| {
            (
                s.style,
                s.text.chars().filter(|c| !c.is_control()).collect(),
            )
        })
        .collect();
    if trim_end {
        while let Some(last) = spans.last_mut() {
            let visible_blank = last.0.bg != Color::Default
                || last.0.attrs & (attr::INVERSE | attr::ANY_UNDERLINE) != 0;
            if visible_blank {
                break;
            }
            let n = last.1.trim_end().len();
            last.1.truncate(n);
            if !last.1.is_empty() {
                break;
            }
            spans.pop();
        }
    }
    let mut out = String::new();
    let mut cur = Style::default();
    for (st, text) in spans {
        if text.is_empty() {
            continue;
        }
        if st != cur {
            out.push_str(&crate::screen::sgr(st));
            cur = st;
        }
        out.push_str(&text);
    }
    if cur != Style::default() {
        out.push_str("\x1b[0m");
    }
    out
}

/// The editor file with colours: the same logical lines as [`text_of`] (so `+N` still lands on
/// the viewer's line), each raw row taken from `styled` (absolute line → row) when its text
/// matches what the archive read returned, else as plain text.
pub fn ansi_text(
    rows: &[(u64, String, bool)],
    styled: &std::collections::HashMap<u64, vk_proto::render::Row>,
) -> String {
    let mut out = String::new();
    let mut line = String::new();
    for (i, (n, text, wrapped)) in rows.iter().enumerate() {
        let last_of_line = !*wrapped || i + 1 == rows.len();
        let styled_row = styled
            .get(n)
            .filter(|r| r.text().trim_end() == text.trim_end());
        match styled_row {
            Some(r) => line.push_str(&row_ansi(r, last_of_line)),
            None if last_of_line => line.push_str(text.trim_end()),
            None => line.push_str(text),
        }
        if last_of_line {
            out.push_str(&line);
            out.push('\n');
            line.clear();
        }
    }
    out
}

/// Private per-user temp directory for editor copies.
#[cfg(not(target_arch = "wasm32"))]
pub fn default_dir() -> PathBuf {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir().join(format!("vibeke-{uid}"))
}

/// Open the viewer for the focused pane (`edit_scrollback`), at `goal` (absolute line) or at
/// the live screen.
pub fn open(app: &mut App, goal: Option<u64>) {
    let Some(pane) = app.focused_pane() else {
        app.toast("no focused pane");
        return;
    };
    let mi = app.cur;
    let title = app.machines[mi]
        .model
        .panes
        .iter()
        .find(|p| p.id == pane)
        .map(|p| format!("{} {}", p.handle, p.display_title()))
        .unwrap_or_else(|| pane.clone());
    app.parity.scrollback_seq += 1;
    let seq = app.parity.scrollback_seq;
    let page = body_rows(app);
    app.scrollback = Some(ScrollbackView {
        machine: mi,
        pane: pane.clone(),
        title,
        rows: Vec::new(),
        lines: Vec::new(),
        loading: true,
        seq,
        truncated: false,
        top: 0,
        goal,
        search: None,
        last_search: None,
        message: None,
        error: None,
        page,
        dir: default_dir(),
        sel: None,
        selecting: false,
        mem_first: None,
        ansi: None,
    });
    app.mode = Mode::Popup(Popup::Scrollback);
    app.command_on(
        mi,
        "pane.read",
        json!({"pane": pane, "source": "archive", "lines": PAGE}),
        Pending::Parity(crate::parity::Reply::Scrollback(Reply::Page { seq })),
    );
}

/// Palette / keymap actions owned here.
pub fn action(app: &mut App, action: &str) -> bool {
    if action != "edit_scrollback" {
        return false;
    }
    // From copy mode: at its view; otherwise at the live screen.
    let goal = match &app.mode {
        Mode::Copy(cm) => Some(cm.view_top_abs()),
        _ => None,
    };
    open(app, goal);
    true
}

/// Join soft-wrapped rows into logical lines (03 §11.3: unwrapped).
pub fn join_rows(rows: &[(u64, String, bool)]) -> Vec<(u64, String)> {
    let mut out: Vec<(u64, String)> = Vec::new();
    let mut cont = false;
    for (n, text, wrapped) in rows {
        if cont && let Some(last) = out.last_mut() {
            last.1.push_str(text);
        } else {
            out.push((*n, text.clone()));
        }
        cont = *wrapped;
    }
    for l in &mut out {
        let t = l.1.trim_end().len();
        l.1.truncate(t);
    }
    out
}

pub fn on_reply(app: &mut App, r: Reply, res: Result<Value, RpcErr>) {
    let Reply::Page { seq } = r;
    let Some(v) = app.scrollback.as_mut().filter(|v| v.seq == seq) else {
        return;
    };
    app.dirty = true;
    let x = match res {
        Ok(x) => x,
        Err(e) => {
            v.loading = false;
            v.error = Some(e.message);
            finish(v);
            return;
        }
    };
    let rows: Vec<(u64, String, bool)> = x
        .get("rows")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|r| {
                    (
                        r.get("n").and_then(Value::as_u64).unwrap_or(0),
                        r.get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        r.get("wrapped").and_then(Value::as_bool).unwrap_or(false),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    // Only rows older than what's loaded (a page may overlap if the pane scrolled).
    let oldest = v.rows.first().map(|r| r.0);
    let mut fresh: Vec<_> = rows
        .into_iter()
        .filter(|r| oldest.is_none_or(|o| r.0 < o))
        .collect();
    let got = !fresh.is_empty();
    fresh.append(&mut v.rows);
    v.rows = fresh;
    if v.mem_first.is_none() {
        v.mem_first = x.get("mem_first").and_then(Value::as_u64);
    }
    let more = x.get("more_before").and_then(Value::as_bool) == Some(true);
    if more && got && v.rows.len() < MAX_ROWS {
        let to = v.rows[0].0;
        let lines = PAGE.min((MAX_ROWS - v.rows.len()) as u64);
        let (mi, pane) = (v.machine, v.pane.clone());
        app.command_on(
            mi,
            "pane.read",
            json!({"pane": pane, "source": "archive", "to": to, "lines": lines}),
            Pending::Parity(crate::parity::Reply::Scrollback(Reply::Page { seq })),
        );
        return;
    }
    v.truncated = more && got;
    v.loading = false;
    finish(v);
}

/// All pages are in: build the logical lines and position the view.
fn finish(v: &mut ScrollbackView) {
    v.lines = join_rows(&v.rows);
    let n = v.lines.len();
    v.top = match v.goal {
        // The logical line containing the goal row.
        Some(g) => v.lines.iter().rposition(|(l, _)| *l <= g).unwrap_or(0),
        // The live screen: the last page.
        None => n.saturating_sub(v.page.max(1)),
    };
}

fn clamp(v: &mut ScrollbackView) {
    v.top = v.top.min(v.lines.len().saturating_sub(1));
}

/// Smart-case search from the line after `top` (or before it, backwards).
pub fn find(v: &mut ScrollbackView, q: &str, back: bool) -> bool {
    let lower = !q.chars().any(char::is_uppercase);
    let norm = |s: &str| {
        if lower {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    let needle = norm(q);
    let n = v.lines.len();
    for step in 1..=n {
        let i = if back {
            (v.top + n - step % n) % n
        } else {
            (v.top + step) % n
        };
        if norm(&v.lines[i].1).contains(&needle) {
            v.top = i;
            v.message = None;
            return true;
        }
    }
    v.message = Some(format!("not found: {q}"));
    false
}

/// The text written to the editor file.
pub fn text_of(v: &ScrollbackView) -> String {
    let mut s = String::new();
    for (_, l) in &v.lines {
        s.push_str(l);
        s.push('\n');
    }
    s
}

/// `$VISUAL`, else `$EDITOR`, split on whitespace.
pub fn editor_from(visual: Option<String>, editor: Option<String>) -> Option<Vec<String>> {
    let cmd = visual
        .filter(|s| !s.trim().is_empty())
        .or(editor.filter(|s| !s.trim().is_empty()))?;
    Some(cmd.split_whitespace().map(str::to_string).collect())
}

/// argv for the editor opening `file` at 1-based `line` (`+N` for editors that take it,
/// `file:N` for helix).
pub fn editor_argv(editor: &[String], file: &Path, line: usize) -> Vec<String> {
    let mut argv = editor.to_vec();
    let base = Path::new(&editor[0])
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let f = file.display().to_string();
    match base {
        "vi" | "vim" | "nvim" | "view" | "nano" | "emacs" | "emacsclient" | "kak" | "micro"
        | "mg" | "joe" => {
            argv.push(format!("+{line}"));
            argv.push(f);
        }
        "hx" | "helix" => argv.push(format!("{f}:{line}")),
        _ => argv.push(f),
    }
    argv
}

/// Write `text` to a new read-only file in the private directory `dir` (0700). The file is
/// removed again if any step fails, and when the returned guard drops.
#[cfg(not(target_arch = "wasm32"))]
pub fn write_private(dir: &Path, pane: &str, text: &str) -> std::io::Result<TempFile> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;
    create_private(
        dir,
        pane,
        |f| f.write_all(text.as_bytes()),
        |p| std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o400)),
    )
}

/// Copies older than this are leftovers of a TUI that was killed (SIGKILL, SIGHUP without
/// unwinding) while an editor had one open; no editor session lasts this long in practice.
#[cfg(not(target_arch = "wasm32"))]
const STALE_AFTER: crate::time::Duration = crate::time::Duration::from_secs(24 * 3600);

/// Remove `scrollback-*.txt` files in `dir` last modified more than `age` ago (best effort).
#[cfg(not(target_arch = "wasm32"))]
fn sweep_stale(dir: &Path, age: crate::time::Duration) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let now = crate::time::SystemTime::now();
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("scrollback-") && name.ends_with(".txt")) {
            continue;
        }
        let old = e
            .metadata()
            .ok()
            .filter(|m| m.is_file())
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|d| d > age);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// [`write_private`] with the write and finishing (chmod) steps supplied by the caller.
#[cfg(not(target_arch = "wasm32"))]
fn create_private(
    dir: &Path,
    pane: &str,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
    finish: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<TempFile> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // A pre-existing directory must be ours and private.
    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    {
        use std::os::unix::fs::MetadataExt;
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(std::io::Error::other(format!(
                "{} is not a private directory",
                dir.display()
            )));
        }
    }
    sweep_stale(dir, STALE_AFTER);
    let safe: String = pane
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let nanos = crate::time::SystemTime::now()
        .duration_since(crate::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("scrollback-{safe}-{nanos:x}.txt"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    // Ours from here on: the guard removes it on any error or unwind below.
    let guard = TempFile::new(path);
    write(&mut f)?;
    drop(f);
    finish(guard.path())?;
    Ok(guard)
}

/// `e`: hand the text to `$VISUAL`/`$EDITOR` (run by the main loop with the TUI suspended).
fn open_editor(app: &mut App) {
    let editor = editor_from(std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok());
    let Some(v) = app.scrollback.as_mut() else {
        return;
    };
    let Some(editor) = editor else {
        v.message = Some("Set $VISUAL or $EDITOR to open the scrollback in an editor".into());
        return;
    };
    start_editor(app, editor);
}

/// Open the editor on the viewer's text; with `copy_mode.editor_include_ansi` the styled
/// in-memory rows are fetched first (`FetchHistory`) and the file keeps their colours.
pub(crate) fn start_editor(app: &mut App, editor: Vec<String>) {
    let ansi = app.config.keys.copy_mode.editor_include_ansi;
    let Some(v) = app.scrollback.as_ref() else {
        return;
    };
    if ansi && v.ansi.is_none() {
        if v.mem_first.is_some() {
            let (mi, pane) = (v.machine, v.pane.clone());
            let req = app.next_req;
            app.next_req += 1;
            app.machines[mi].send(vk_proto::render::ClientFrame::FetchHistory {
                req,
                pane,
                start: 0,
                count: 0,
            });
            if let Some(v) = app.scrollback.as_mut() {
                v.ansi = Some(AnsiFetch {
                    req,
                    rows_asked: false,
                    argv: editor,
                });
                v.message = Some("loading colours for the editor…".into());
            }
            return;
        }
        // An older server doesn't say where memory starts: plain text it is.
        let text = text_of(v);
        launch(
            app,
            editor,
            text,
            "Opened in the editor without colours (the server doesn't report mem_first)",
        );
        return;
    }
    let text = text_of(v);
    launch(
        app,
        editor,
        text,
        "Opened in the editor (a read-only copy, deleted on exit)",
    );
}

fn launch(app: &mut App, editor: Vec<String>, text: String, msg: &str) {
    let Some(v) = app.scrollback.as_mut() else {
        return;
    };
    match write_private(&v.dir, &v.pane, &text) {
        Ok(file) => {
            let argv = editor_argv(&editor, &file, v.top + 1);
            v.message = Some(msg.to_string());
            let mi = v.machine;
            crate::popup_pane::editor(app, mi, argv, file);
        }
        Err(e) => v.message = Some(format!("couldn't write the temp file: {e}")),
    }
}

/// A `History` frame for the editor's styled-rows fetch; true when it was ours.
pub(crate) fn on_history(
    app: &mut App,
    mi: usize,
    pane: &str,
    req: u64,
    start: u32,
    total: u32,
    lines: &[vk_proto::render::Row],
) -> bool {
    let Some(v) = app.scrollback.as_ref() else {
        return false;
    };
    let Some(a) = v.ansi.as_ref().filter(|a| a.req == req) else {
        return false;
    };
    if v.machine != mi || v.pane != pane {
        return false;
    }
    if !a.rows_asked && lines.is_empty() && total > 0 {
        let count = total.min(ANSI_ROWS);
        let r = app.next_req;
        app.next_req += 1;
        app.machines[mi].send(vk_proto::render::ClientFrame::FetchHistory {
            req: r,
            pane: pane.to_string(),
            start: total - count,
            count,
        });
        if let Some(a) = app.scrollback.as_mut().and_then(|v| v.ansi.as_mut()) {
            a.req = r;
            a.rows_asked = true;
        }
        return true;
    }
    // Absolute line of in-memory row k is mem_first + k; the screen follows the history.
    let base = v.mem_first.unwrap_or(0);
    let mut styled: std::collections::HashMap<u64, vk_proto::render::Row> = lines
        .iter()
        .enumerate()
        .map(|(k, r)| (base + start as u64 + k as u64, r.clone()))
        .collect();
    if let Some(buf) = app.machines[mi].panes.get(pane) {
        for (j, r) in buf.lines.iter().enumerate() {
            styled.insert(base + total as u64 + j as u64, r.clone());
        }
    }
    let text = ansi_text(&v.rows, &styled);
    let argv = a.argv.clone();
    if let Some(v) = app.scrollback.as_mut() {
        v.ansi = None;
    }
    launch(
        app,
        argv,
        text,
        "Opened in the editor with colours (a read-only copy, deleted on exit)",
    );
    true
}

/// Run an [`External`] with inherited stdio (the caller suspended the TUI), then delete its file
/// (dropping the [`External`] would too; this deletes it as soon as the program exits).
/// Tests: a temp file guard for an existing path.
#[cfg(test)]
pub(crate) fn test_temp_file(p: PathBuf) -> TempFile {
    TempFile::new(p)
}

pub fn run_external(x: &External) -> Result<(), String> {
    let status = std::process::Command::new(&x.argv[0])
        .args(&x.argv[1..])
        .status();
    remove_file(x.file.path());
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{} exited with {s}", x.argv[0])),
        Err(e) => Err(format!("couldn't start {}: {e}", x.argv[0])),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn remove_file(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    let _ = std::fs::remove_file(p);
}

pub fn key(app: &mut App, ev: KeyEvent) {
    if ev.kind == KeyKind::Release {
        return;
    }
    let page = body_rows(app);
    let Some(v) = app.scrollback.as_mut() else {
        app.mode = Mode::Normal;
        return;
    };
    v.page = page;
    // The popup dispatcher took the mode; keep the viewer open unless closed below.
    app.mode = Mode::Popup(Popup::Scrollback);
    app.dirty = true;
    if let Some(mut q) = v.search.take() {
        match ev.key {
            Key::Named(NamedKey::Escape) => {}
            Key::Named(NamedKey::Enter) => {
                if !q.is_empty() {
                    find(v, &q, false);
                    v.last_search = Some(q);
                }
            }
            Key::Named(NamedKey::Backspace) => {
                q.pop();
                v.search = Some(q);
            }
            Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => {
                q.push(c);
                v.search = Some(q);
            }
            _ => v.search = Some(q),
        }
        return;
    }
    v.message = None;
    match ev.key {
        Key::Named(NamedKey::Escape) if v.sel.is_some() => v.sel = None,
        Key::Char('y') | Key::Named(NamedKey::Enter) if v.sel.is_some() => {
            let text = selection_text(v);
            v.sel = None;
            if !text.is_empty() {
                app.copy_text(&text);
            }
            return;
        }
        Key::Named(NamedKey::Escape) | Key::Char('q') => {
            app.scrollback = None;
            app.mode = Mode::Normal;
            return;
        }
        Key::Char('j') | Key::Named(NamedKey::Down) => v.top += 1,
        Key::Char('k') | Key::Named(NamedKey::Up) => v.top = v.top.saturating_sub(1),
        Key::Char(' ') | Key::Named(NamedKey::PageDown) => v.top += page,
        Key::Char('d') if ev.mods.ctrl() => v.top += page / 2,
        Key::Char('u') if ev.mods.ctrl() => v.top = v.top.saturating_sub(page / 2),
        Key::Named(NamedKey::PageUp) | Key::Char('b') => v.top = v.top.saturating_sub(page),
        Key::Char('g') | Key::Named(NamedKey::Home) => v.top = 0,
        Key::Char('G') | Key::Named(NamedKey::End) => v.top = v.lines.len().saturating_sub(page),
        Key::Char('/') => v.search = Some(String::new()),
        Key::Char(c @ ('n' | 'N')) => match v.last_search.clone() {
            Some(q) => {
                find(v, &q, c == 'N');
            }
            None => v.message = Some("no search yet — / to search".into()),
        },
        Key::Char('e') => {
            if v.loading {
                v.message = Some("still loading…".into());
            } else {
                open_editor(app);
            }
            return;
        }
        _ => {}
    }
    clamp(v);
}

/// Display rows for logical line `l` wrapped at `w` columns.
fn wrap(l: &str, w: usize) -> Vec<String> {
    if l.is_empty() || w == 0 {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cw = 0;
    for c in l.chars() {
        let cwid = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if cw + cwid > w {
            out.push(std::mem::take(&mut cur));
            cw = 0;
        }
        cur.push(c);
        cw += cwid;
    }
    out.push(cur);
    out
}

pub fn draw(app: &App, g: &mut Grid) {
    let Some(v) = &app.scrollback else {
        return;
    };
    let t = app.theme;
    let mut a = Area::open(app, g, &format!("scrollback · {} · read-only", v.title));
    let range = match (v.lines.first(), v.lines.last()) {
        (Some(f), Some(l)) => format!(
            "{} lines · history lines {}–{}{}",
            v.lines.len(),
            f.0,
            l.0,
            if v.truncated {
                format!(" · older lines not loaded (limit {MAX_ROWS} rows)")
            } else {
                String::new()
            }
        ),
        _ if v.loading => "loading…".into(),
        _ => "no history".into(),
    };
    a.line(&range, t.dim());
    if let Some(e) = &v.error {
        a.line(&format!("✗ {e}"), t.s(t.red));
    }
    let status = match (&v.search, &v.message) {
        (Some(q), _) => Some((format!("/{q}▏"), t.bold(t.fg))),
        (None, Some(m)) => Some((m.clone(), t.s(t.yellow))),
        _ => None,
    };
    let body_bottom = a.bottom().saturating_sub(u16::from(status.is_some()));
    let w = a.r.w.saturating_sub(2) as usize;
    let x = a.r.x + 1;
    let mut y = a.y;
    let q = v.last_search.as_deref().filter(|q| !q.is_empty());
    let sel = v.sel.map(ordered);
    for (li, (_, l)) in v.lines.iter().enumerate().skip(v.top) {
        let hit = q.is_some_and(|q| {
            if q.chars().any(char::is_uppercase) {
                l.contains(q)
            } else {
                l.to_lowercase().contains(&q.to_lowercase())
            }
        });
        let mut off = 0;
        for part in wrap(l, w) {
            if y >= body_bottom {
                break;
            }
            a.g.put_str(
                x,
                y,
                &part,
                if hit { t.s(t.accent) } else { t.text() },
                w as u16,
            );
            // The mouse selection, highlighted cell by cell.
            if let Some((s0, s1)) = sel {
                let mut cx = x;
                for (i, c) in part.chars().enumerate() {
                    let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as u16;
                    if s0 <= (li, off + i) && (li, off + i) <= s1 {
                        a.g.put_str(cx, y, &c.to_string(), t.sel(t.fg), cw.max(1));
                    }
                    cx += cw;
                }
            }
            off += part.chars().count();
            y += 1;
        }
        if y >= body_bottom {
            break;
        }
    }
    if let Some((s, st)) = status {
        a.g.put_str(x, body_bottom, &s, st, w as u16);
    }
    a.footer(
        if v.sel.is_some() && !v.selecting {
            "y copy selection · esc clear · drag select · double/triple click word/line"
        } else {
            "j/k scroll · space/b page · g/G top/bottom · / search · n/N next · e open in $EDITOR (read-only copy) · drag to copy · esc close"
        },
        t.dim(),
    );
}

/// Body text geometry of the viewer: (x, first row, row past the last, width).
fn body_geom(app: &App, v: &ScrollbackView) -> (u16, u16, u16, usize) {
    let a = app.pane_area();
    let top = a.y + 2 + u16::from(v.error.is_some());
    let status = v.search.is_some() || v.message.is_some();
    let bottom = (a.y + a.h.saturating_sub(1)).saturating_sub(u16::from(status));
    (
        a.x + 1,
        top,
        bottom.max(top),
        a.w.saturating_sub(2) as usize,
    )
}

type Pos = (usize, usize);

fn ordered((a, b): (Pos, Pos)) -> (Pos, Pos) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The text position (logical line, char index) under screen cell (`x`, `y`), clamped to the
/// shown text; `None` when nothing is loaded.
fn hit(app: &App, v: &ScrollbackView, x: u16, y: u16) -> Option<Pos> {
    let (x0, top, bottom, w) = body_geom(app, v);
    if v.lines.is_empty() {
        return None;
    }
    let want = y.clamp(top, bottom.saturating_sub(1).max(top)) - top;
    let col = x.saturating_sub(x0) as usize;
    let mut row = 0u16;
    let mut last = (v.top.min(v.lines.len() - 1), 0);
    for (li, (_, l)) in v.lines.iter().enumerate().skip(v.top) {
        let mut off = 0;
        for part in wrap(l, w) {
            let n = part.chars().count();
            if row == want {
                let mut cw = 0;
                let mut i = 0;
                for c in part.chars() {
                    let wd = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                    if cw + wd > col {
                        break;
                    }
                    cw += wd;
                    i += 1;
                }
                return Some((li, off + i.min(n.saturating_sub(1))));
            }
            last = (li, off + n.saturating_sub(1));
            off += n;
            row += 1;
        }
    }
    Some(last)
}

/// Selected text: logical lines joined with newlines, trailing blanks trimmed.
pub fn selection_text(v: &ScrollbackView) -> String {
    let Some((s0, s1)) = v.sel.map(ordered) else {
        return String::new();
    };
    let mut out = Vec::new();
    for li in s0.0..=s1.0.min(v.lines.len().saturating_sub(1)) {
        let chars: Vec<char> = v.lines[li].1.chars().collect();
        let from = if li == s0.0 { s0.1 } else { 0 };
        let to = if li == s1.0 { s1.1 + 1 } else { chars.len() };
        let part: String = chars[from.min(chars.len())..to.min(chars.len())]
            .iter()
            .collect();
        out.push(part.trim_end().to_string());
    }
    out.join("\n")
}

/// The mouse over the viewer (03 §11.1): a drag selects (copied on release with
/// `copy_on_select`, else kept for `y`), a double/triple click selects a word/line, dragging
/// past the top/bottom scrolls, the wheel scrolls. True while the viewer is open (it is modal).
pub fn on_mouse(app: &mut App, me: &crate::event::MouseEvent) -> bool {
    use crate::event::{MouseButton as B, MouseEventKind as K};
    if !matches!(app.mode, Mode::Popup(Popup::Scrollback)) {
        return false;
    }
    let Some(v) = app.scrollback.as_mut() else {
        return false;
    };
    app.dirty = true;
    match me.kind {
        K::Down(B::Left) => {
            let v = app.scrollback.as_ref().unwrap();
            let Some(p) = hit(app, v, me.column, me.row) else {
                return true;
            };
            let n = crate::selection::click_count(
                &mut app.parity.selection,
                "\u{0}scrollback",
                me.column,
                me.row,
                crate::time::Instant::now(),
            );
            let v = app.scrollback.as_mut().unwrap();
            let line = &v.lines[p.0].1;
            v.selecting = true;
            v.sel = Some(match n {
                // Anchor only: a drag makes it a selection.
                1 => (p, p),
                2 => word_at(line, p),
                _ => ((p.0, 0), (p.0, line.chars().count().saturating_sub(1))),
            });
            app.parity.selection.dragged = n >= 2;
        }
        K::Drag(B::Left) => {
            if !v.selecting {
                return true;
            }
            let (_, top, bottom, _) = body_geom(app, app.scrollback.as_ref().unwrap());
            let v = app.scrollback.as_mut().unwrap();
            if me.row < top {
                v.top = v.top.saturating_sub(1);
            } else if me.row >= bottom {
                v.top = (v.top + 1).min(v.lines.len().saturating_sub(1));
            }
            let pos = hit(app, app.scrollback.as_ref().unwrap(), me.column, me.row);
            let v = app.scrollback.as_mut().unwrap();
            if let (Some(p), Some((a, _))) = (pos, v.sel) {
                v.sel = Some((a, p));
            }
            app.parity.selection.dragged = true;
        }
        K::Up(B::Left) => {
            if !v.selecting {
                return true;
            }
            v.selecting = false;
            let dragged = std::mem::take(&mut app.parity.selection.dragged);
            let v = app.scrollback.as_mut().unwrap();
            if !dragged {
                v.sel = None;
            } else if app.config.clipboard.copy_on_select {
                let text = selection_text(v);
                v.sel = None;
                if !text.is_empty() {
                    app.copy_text(&text);
                }
            }
        }
        K::ScrollUp => v.top = v.top.saturating_sub(3),
        K::ScrollDown => v.top = (v.top + 3).min(v.lines.len().saturating_sub(1)),
        _ => {}
    }
    true
}

/// The word (or blank run, or single other character) around char `p.1` of `line`.
fn word_at(line: &str, p: Pos) -> (Pos, Pos) {
    let chars: Vec<String> = line.chars().map(String::from).collect();
    if chars.is_empty() {
        return (p, p);
    }
    let i = p.1.min(chars.len() - 1);
    let cls = crate::selection::word_class(&chars[i]);
    let same = |j: usize| cls != 2 && crate::selection::word_class(&chars[j]) == cls;
    let (mut a, mut b) = (i, i);
    while a > 0 && same(a - 1) {
        a -= 1;
    }
    while b + 1 < chars.len() && same(b + 1) {
        b += 1;
    }
    ((p.0, a), (p.0, b))
}

/// Body rows of the viewer (title, range line and footer excluded).
fn body_rows(app: &App) -> usize {
    (app.pane_area().h.saturating_sub(3) as usize).max(1)
}

#[cfg(test)]
#[path = "scrollback_tests.rs"]
mod tests;

#[cfg(test)]
mod cleanup_tests {
    use super::*;

    #[test]
    fn leftovers_of_a_killed_tui_are_swept_on_the_next_write() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("priv");
        let first = write_private(&dir, "p1", "one").unwrap();
        // A killed TUI never ran the guard: the file stays behind.
        let left = first.path().to_path_buf();
        first.persist();
        let other = dir.join("notes.txt");
        std::fs::write(&other, "keep").unwrap();
        // Not stale yet: kept.
        let second = write_private(&dir, "p2", "two").unwrap();
        assert!(left.exists());
        drop(second);
        // Stale: swept (other files in the directory are never touched).
        std::thread::sleep(crate::time::Duration::from_millis(20));
        sweep_stale(&dir, crate::time::Duration::ZERO);
        assert!(!left.exists());
        assert!(other.exists());
    }

    fn entries(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect()
    }

    #[test]
    fn a_failed_write_removes_the_partial_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("priv");
        let err = create_private(
            &dir,
            "p1",
            |f| {
                use std::io::Write as _;
                f.write_all(b"partial")?;
                Err(std::io::Error::other("disk full"))
            },
            |_| panic!("finish must not run after a failed write"),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "disk full");
        assert!(entries(&dir).is_empty());
    }

    #[test]
    fn a_failed_chmod_removes_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("priv");
        let mut seen = None;
        let err = create_private(
            &dir,
            "p1",
            |f| {
                use std::io::Write as _;
                f.write_all(b"text")
            },
            |p| {
                assert!(p.exists());
                seen = Some(p.to_path_buf());
                Err(std::io::Error::other("chmod refused"))
            },
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "chmod refused");
        assert!(!seen.unwrap().exists());
        assert!(entries(&dir).is_empty());
    }

    #[test]
    fn dropping_the_guard_on_early_return_or_panic_removes_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("priv");
        // Early return after the file was made, before handing it off.
        let mut made = None;
        let r = (|| -> std::io::Result<External> {
            let f = write_private(&dir, "p1", "x")?;
            made = Some(f.path().to_path_buf());
            Err(std::io::Error::other("later step failed"))
        })();
        assert!(r.is_err());
        assert!(!made.unwrap().exists());
        // A panic between creation and handoff.
        let d = dir.clone();
        let r = std::panic::catch_unwind(move || {
            let _f = write_private(&d, "p1", "x").unwrap();
            panic!("interrupted");
        });
        assert!(r.is_err());
        assert!(entries(&dir).is_empty());
        // Queued for the editor but never run (the app quit first).
        let file = write_private(&dir, "p1", "x").unwrap();
        let path = file.path().to_path_buf();
        let x = External {
            argv: vec!["true".into()],
            file,
        };
        assert!(path.exists());
        drop(x);
        assert!(!path.exists());
    }

    #[test]
    fn on_success_the_file_lives_until_the_consumer_is_done() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("priv");
        let file = write_private(&dir, "p1", "hello\n").unwrap();
        let path = file.path().to_path_buf();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n");
        // The consumer reads the file while it runs; it is gone once it exits.
        let out = tmp.path().join("seen");
        let x = External {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "cat \"$1\" > \"$2\"".into(),
                "sh".into(),
                path.display().to_string(),
                out.display().to_string(),
            ],
            file,
        };
        assert!(path.exists());
        run_external(&x).unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "hello\n");
        assert!(!path.exists());
        drop(x);
        // A failing consumer still cleans up.
        let file = write_private(&dir, "p1", "x").unwrap();
        let path = file.path().to_path_buf();
        let x = External {
            argv: vec!["/bin/sh".into(), "-c".into(), "exit 3".into()],
            file,
        };
        assert!(run_external(&x).is_err());
        assert!(!path.exists());
        // `persist` hands ownership to the caller: nothing is removed on drop.
        let kept = write_private(&dir, "p1", "x").unwrap().persist();
        assert!(kept.exists());
        remove_file(&kept);
        assert!(entries(&dir).is_empty());
    }
}

#[cfg(target_arch = "wasm32")]
pub fn default_dir() -> PathBuf {
    PathBuf::new()
}
#[cfg(target_arch = "wasm32")]
pub fn write_private(_: &Path, _: &str, _: &str) -> std::io::Result<TempFile> {
    Err(std::io::Error::other(
        "Use Copy to copy scrollback in the browser",
    ))
}
#[cfg(target_arch = "wasm32")]
fn remove_file(_: &Path) {}
