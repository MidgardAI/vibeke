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
#[derive(Debug, Clone, PartialEq)]
pub struct External {
    pub argv: Vec<String>,
    /// Deleted after the program exits.
    pub file: PathBuf,
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
}

/// Private per-user temp directory for editor copies.
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

/// Write `text` to a new read-only file in the private directory `dir` (0700).
pub fn write_private(dir: &Path, pane: &str, text: &str) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
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
    let safe: String = pane
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("scrollback-{safe}-{nanos:x}.txt"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(text.as_bytes())?;
    drop(f);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400))?;
    Ok(path)
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
    match write_private(&v.dir, &v.pane, &text_of(v)) {
        Ok(file) => {
            let argv = editor_argv(&editor, &file, v.top + 1);
            v.message = Some("Opened in the editor (a read-only copy, deleted on exit)".into());
            app.external = Some(External { argv, file });
        }
        Err(e) => v.message = Some(format!("couldn't write the temp file: {e}")),
    }
}

/// Run an [`External`] with inherited stdio (the caller suspended the TUI), then delete its file.
pub fn run_external(x: &External) -> Result<(), String> {
    let status = std::process::Command::new(&x.argv[0])
        .args(&x.argv[1..])
        .status();
    remove_file(&x.file);
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{} exited with {s}", x.argv[0])),
        Err(e) => Err(format!("couldn't start {}: {e}", x.argv[0])),
    }
}

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
    for (_, l) in v.lines.iter().skip(v.top) {
        let hit = q.is_some_and(|q| {
            if q.chars().any(char::is_uppercase) {
                l.contains(q)
            } else {
                l.to_lowercase().contains(&q.to_lowercase())
            }
        });
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
        "j/k scroll · space/b page · g/G top/bottom · / search · n/N next · e open in $EDITOR (read-only copy) · esc close",
        t.dim(),
    );
}

/// Body rows of the viewer (title, range line and footer excluded).
fn body_rows(app: &App) -> usize {
    (app.pane_area().h.saturating_sub(3) as usize).max(1)
}

#[cfg(test)]
#[path = "scrollback_tests.rs"]
mod tests;
