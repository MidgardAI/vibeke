//! Screenshot gallery and screenshot pane (06 B8; server: `screenshot.list/get/open/delete`,
//! `browser.diff`, event `screenshot.captured`).
//!
//! - **Gallery** (`:screenshots`, agent peek `p` for that agent's screenshots): a full pane-area
//!   view, newest first. Each row shows the handle, age, environment label ("devbox · headless ·
//!   fresh context" vs "your browser pane · profile devbox"), binding (`bound` / `illustrative`
//!   with the reason), code state and URL. `enter` shows the selected screenshot full size.
//! - **Screenshot pane** (`:screenshot_pane`): the same view in full-image mode that follows new
//!   screenshots as they are captured (`f` toggles following).
//! - **Images**: drawn with kitty graphics (a PNG transmitted once per image and size, shown
//!   through unicode placeholders like the browser pane's tiles) when the host supports it.
//!   Screenshots on the local machine are read from disk; a remote machine's image is fetched
//!   only on an explicit `v` (never sent over the link unasked). Without graphics: metadata and
//!   `o` **open image locally** (the OS opener, only on that explicit key).
//! - **Diff**: `space` marks a base, `d` diffs it (or the next older screenshot) against the
//!   selected one with `browser.diff`; different environments are refused unless `!` forces.
//! - `x` delete (confirm; a screenshot kept for a review acceptance needs `!`), `t` scope
//!   (all / this task), `L` open the live preview in a browser pane, `y` copy handle + path.
//! - **📷 counters**: `screenshot.captured` events count per requesting pane; sidebar agent rows
//!   and unfocused pane corners show `📷N` until the gallery is opened for them.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::drafts::{Area, arr, now_ms, st};
use crate::draw::truncate;
use crate::screen::Grid;
use base64::Engine as _;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

/// Kitty image id for the gallery (above every browser-pane tile id, below 2^24).
pub const IMAGE_ID: u32 = 16_700_001;
/// Largest image the gallery decodes/transmits.
const MAX_IMAGE: usize = 16 << 20;
const CACHE: usize = 8;

#[derive(Debug, Clone)]
pub enum Reply {
    List {
        view: u64,
    },
    Image {
        view: u64,
        key: String,
    },
    Diff {
        view: u64,
    },
    Deleted {
        view: u64,
    },
    Opened,
    /// The newest screenshot of a preview (sidebar thumbnail).
    Thumb {
        machine: usize,
        preview: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Scope {
    All,
    Task(String),
    Pane(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Shot {
    pub id: String,
    pub handle: String,
    pub label: String,
    pub url: String,
    pub title: String,
    pub created_at_ms: i64,
    pub binding: String,
    pub binding_reason: String,
    pub env_kind: String,
    pub head_sha: Option<String>,
    pub dirty_state: Option<String>,
    pub task: Option<String>,
    pub pane: Option<String>,
    pub preview: Option<String>,
    pub path: String,
    pub exists: bool,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
}

fn opt(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl Shot {
    pub fn parse(v: &Value) -> Option<Shot> {
        let id = opt(v, "id")?;
        let code = v.get("code").cloned().unwrap_or(Value::Null);
        Some(Shot {
            id,
            handle: st(v, "handle").into(),
            label: st(v, "label").into(),
            url: opt(v, "final_url").unwrap_or_else(|| st(v, "url").into()),
            title: st(v, "title").into(),
            created_at_ms: v.get("created_at_ms").and_then(Value::as_i64).unwrap_or(0),
            binding: st(v, "binding").into(),
            binding_reason: st(v, "binding_reason").into(),
            env_kind: v
                .pointer("/environment/kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            head_sha: opt(&code, "head_sha"),
            dirty_state: opt(&code, "dirty_state"),
            task: opt(v, "task"),
            pane: opt(v, "pane").or_else(|| {
                v.pointer("/taken_by/pane")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            }),
            preview: opt(v, "preview"),
            path: st(v, "path_on_machine").into(),
            exists: v.get("exists").and_then(Value::as_bool).unwrap_or(true),
            width: v.get("width").and_then(Value::as_u64).unwrap_or(0) as u32,
            height: v.get("height").and_then(Value::as_u64).unwrap_or(0) as u32,
            bytes: v.get("bytes").and_then(Value::as_u64).unwrap_or(0),
        })
    }

    pub fn code_text(&self) -> String {
        match (&self.head_sha, self.dirty_state.as_deref()) {
            (Some(h), Some("dirty")) => format!("{} + uncommitted changes", &h[..h.len().min(8)]),
            (Some(h), Some("unknown")) => format!("{} (dirty state unknown)", &h[..h.len().min(8)]),
            (Some(h), _) => h[..h.len().min(8)].to_string(),
            (None, _) => "no checkout".into(),
        }
    }

    pub fn binding_text(&self) -> String {
        match self.binding.as_str() {
            "bound" => "bound to this code".into(),
            "" => "binding unknown".into(),
            b if self.binding_reason.is_empty() => b.to_string(),
            b => format!("{b}: {}", self.binding_reason),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum View {
    List,
    Full,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiffView {
    pub a: Shot,
    pub b: Shot,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub env_mismatch: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Gallery {
    pub id: u64,
    pub machine: usize,
    pub scope: Scope,
    pub shots: Vec<Shot>,
    pub sel: usize,
    pub view: View,
    pub follow: bool,
    /// Screenshot pane: `esc` closes instead of returning to the list.
    pub pane_mode: bool,
    pub mark: Option<String>,
    pub diff: Option<DiffView>,
    pub confirm_delete: Option<String>,
    pub force_delete: Option<String>,
    pub loading: bool,
    pub notice: Option<String>,
    pub error: Option<String>,
    pub origin_peek: Option<String>,
}

impl Gallery {
    pub fn cur(&self) -> Option<&Shot> {
        self.shots.get(self.sel)
    }
}

/// Client state: the open view, 📷 counters, loaded images and what the terminal holds.
#[derive(Debug, Default)]
pub struct GalleryState {
    pub view: Option<Gallery>,
    /// (machine, pane) → screenshots captured since the gallery was last opened for it.
    pub counts: HashMap<(usize, String), u32>,
    /// Image bytes by key (`<machine>:<shot id>` or `<machine>:diff:<blob>`), bounded.
    pub images: Vec<(String, Vec<u8>)>,
    pub fetching: HashSet<String>,
    /// (key, x, y, cols, rows) of the image currently transmitted with [`IMAGE_ID`].
    pub sent: Option<(String, u16, u16, u16, u16)>,
    /// Files handed to the OS opener (tests read this instead of spawning).
    pub opened: Vec<String>,
    /// Directory for `t=t` temp files (`None`: `$TMPDIR/vibeke-gfx-<uid>`; tests point it at a
    /// temp dir of their own).
    pub gfx_dir: Option<std::path::PathBuf>,
}

impl GalleryState {
    pub fn image(&self, key: &str) -> Option<&[u8]> {
        self.images
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, d)| d.as_slice())
    }
    fn put_image(&mut self, key: String, data: Vec<u8>) {
        self.images.retain(|(k, _)| *k != key);
        self.images.push((key, data));
        if self.images.len() > CACHE {
            self.images.remove(0);
        }
    }
}

// ---- counters ----------------------------------------------------------------------------------------

/// The 📷 counter for a pane (sidebar rows, pane corners).
pub fn badge(app: &App, mi: usize, pane: &str) -> Option<u32> {
    app.gallery
        .counts
        .get(&(mi, pane.to_string()))
        .copied()
        .filter(|n| *n > 0)
}

/// A pushed `screenshot.captured`.
pub fn on_captured(app: &mut App, mi: usize, v: &Value) {
    if let Some(p) = v["subject"]["pane"].as_str() {
        let open_here = app.gallery.view.as_ref().is_some_and(|g| {
            g.machine == mi && (g.scope == Scope::All || g.scope == Scope::Pane(p.to_string()))
        });
        if !open_here {
            *app.gallery.counts.entry((mi, p.to_string())).or_insert(0) += 1;
        }
    }
    if app.gallery.view.as_ref().is_some_and(|g| g.machine == mi) {
        refresh(app);
    }
}

pub fn on_deleted(app: &mut App, mi: usize) {
    if app.gallery.view.as_ref().is_some_and(|g| g.machine == mi) {
        refresh(app);
    }
}

// ---- opening -------------------------------------------------------------------------------------------

pub fn action(app: &mut App, action: &str) -> bool {
    let mi = app.cur;
    match action {
        "screenshots" | "gallery" => open(app, mi, Scope::All, View::List, false),
        "screenshot_pane" => open(app, mi, Scope::All, View::Full, true),
        _ => return false,
    }
    true
}

pub fn open(app: &mut App, mi: usize, scope: Scope, view: View, pane_mode: bool) {
    let id = app.next_ui_id();
    match &scope {
        Scope::Pane(p) => {
            app.gallery.counts.remove(&(mi, p.clone()));
        }
        _ => app.gallery.counts.retain(|(m, _), _| *m != mi),
    }
    app.gallery.view = Some(Gallery {
        id,
        machine: mi,
        scope,
        shots: Vec::new(),
        sel: 0,
        view,
        follow: pane_mode,
        pane_mode,
        mark: None,
        diff: None,
        confirm_delete: None,
        force_delete: None,
        loading: true,
        notice: None,
        error: None,
        origin_peek: None,
    });
    app.mode = Mode::Popup(Popup::Gallery);
    refresh(app);
}

/// Peek `p`: the agent's screenshots.
pub fn open_from_peek(app: &mut App, pane: &str) {
    let mi = app.cur;
    open(app, mi, Scope::Pane(pane.into()), View::List, false);
    if let Some(g) = &mut app.gallery.view {
        g.origin_peek = Some(pane.into());
    }
}

pub fn refresh(app: &mut App) {
    let Some(g) = &mut app.gallery.view else {
        return;
    };
    g.loading = true;
    let mut p = json!({"limit": 100});
    if let Scope::Task(t) = &g.scope {
        p["task"] = json!(t);
    }
    let (mi, id) = (g.machine, g.id);
    app.command_on(
        mi,
        "screenshot.list",
        p,
        Pending::Gallery(Reply::List { view: id }),
    );
}

fn close(app: &mut App) {
    let peek = app.gallery.view.take().and_then(|g| g.origin_peek);
    app.mode = match peek {
        Some(p) => Mode::Popup(Popup::Peek { pane: p }),
        None => Mode::Normal,
    };
}

fn image_key(mi: usize, id: &str) -> String {
    format!("{mi}:{id}")
}

/// Load the selected (or diff) image when that needs no transfer: local machines read the PNG
/// from disk. Remote images wait for an explicit `v`.
pub fn ensure_local_image(app: &mut App) {
    if crate::browser::gfx(app) != crate::browser::Gfx::Kitty {
        return;
    }
    let Some(g) = &app.gallery.view else {
        return;
    };
    if !app.machines[g.machine].local {
        return;
    }
    let want: Option<(String, String)> = match &g.diff {
        Some(d) => d.result.as_ref().map(|r| {
            (
                image_key(g.machine, &format!("diff:{}", st(r, "blob"))),
                st(r, "path_on_machine").to_string(),
            )
        }),
        None if g.view == View::Full => g
            .cur()
            .filter(|s| s.exists && !s.path.is_empty())
            .map(|s| (image_key(g.machine, &s.id), s.path.clone())),
        None => None,
    };
    let Some((key, path)) = want else { return };
    if app.gallery.image(&key).is_some() || path.is_empty() {
        return;
    }
    match std::fs::metadata(&path) {
        Ok(m) if m.len() as usize <= MAX_IMAGE => {
            if let Ok(data) = std::fs::read(&path) {
                app.gallery.put_image(key, data);
            }
        }
        _ => {}
    }
}

/// `v`: fetch the selected image from its machine (explicit for remote machines).
fn fetch_image(app: &mut App, g: &Gallery) {
    if let Some(d) = &g.diff {
        let Some(r) = &d.result else { return };
        let key = image_key(g.machine, &format!("diff:{}", st(r, "blob")));
        if app.gallery.fetching.insert(key.clone()) {
            app.command_on(
                g.machine,
                "browser.diff",
                json!({"a": d.a.id, "b": d.b.id, "inline": true, "force": d.env_mismatch}),
                Pending::Gallery(Reply::Image { view: g.id, key }),
            );
        }
        return;
    }
    let Some(s) = g.cur() else { return };
    let key = image_key(g.machine, &s.id);
    if app.gallery.image(&key).is_some() {
        return;
    }
    if app.gallery.fetching.insert(key.clone()) {
        app.command_on(
            g.machine,
            "screenshot.get",
            json!({"id": s.id, "inline": true}),
            Pending::Gallery(Reply::Image { view: g.id, key }),
        );
    }
}

fn os_open(app: &mut App, path: &str) {
    app.gallery.opened.push(path.to_string());
    if !cfg!(test) {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = std::process::Command::new(opener)
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    app.toast(format!("opened {path}"));
}

/// `o`: open the image with the OS opener. Local: the blob file itself; remote: fetched
/// (`screenshot.open`, explicit) into `$TMPDIR/vibeke-screenshots/<id>.png` first.
fn open_locally(app: &mut App, g: &Gallery) {
    let Some(s) = g.cur() else { return };
    if app.machines[g.machine].local {
        if s.exists {
            let p = s.path.clone();
            os_open(app, &p);
        } else {
            app.toast("the image file is gone (retention)");
        }
        return;
    }
    app.command_on(
        g.machine,
        "screenshot.open",
        json!({"id": s.id}),
        Pending::Gallery(Reply::Opened),
    );
    app.toast(format!(
        "fetching {} from {}…",
        s.handle, app.machines[g.machine].label
    ));
}

fn start_diff(app: &mut App, g: &mut Gallery, force: bool) {
    let Some(b) = g.cur().cloned() else { return };
    let a = match &g.mark {
        Some(m) if *m != b.id => g.shots.iter().find(|s| &s.id == m).cloned(),
        _ => g.shots.get(g.sel + 1).cloned(),
    };
    let Some(a) = a else {
        g.notice = Some("Mark a base with space (or select a newer screenshot), then d".into());
        return;
    };
    let mut p = json!({"a": a.id, "b": b.id});
    if force {
        p["force"] = json!(true);
    }
    g.diff = Some(DiffView {
        a,
        b,
        result: None,
        error: None,
        env_mismatch: force,
    });
    app.command_on(
        g.machine,
        "browser.diff",
        p,
        Pending::Gallery(Reply::Diff { view: g.id }),
    );
}

// ---- keys ------------------------------------------------------------------------------------------------

pub fn key(app: &mut App, ev: KeyEvent) {
    let Some(mut g) = app.gallery.view.take() else {
        app.mode = Mode::Normal;
        return;
    };
    app.mode = Mode::Popup(Popup::Gallery);
    if ev.kind == KeyKind::Release {
        app.gallery.view = Some(g);
        return;
    }
    g.notice = None;
    if let Some(id) = g.confirm_delete.take() {
        if matches!(ev.key, Key::Char('y' | 'Y')) {
            app.command_on(
                g.machine,
                "screenshot.delete",
                json!({"id": id}),
                Pending::Gallery(Reply::Deleted { view: g.id }),
            );
        } else {
            g.notice = Some("Not deleted".into());
        }
        app.gallery.view = Some(g);
        return;
    }
    if let Some(id) = g.force_delete.take() {
        if ev.key == Key::Char('!') {
            app.command_on(
                g.machine,
                "screenshot.delete",
                json!({"id": id, "force": true}),
                Pending::Gallery(Reply::Deleted { view: g.id }),
            );
        } else {
            g.notice = Some("Kept (referenced by a review acceptance)".into());
        }
        app.gallery.view = Some(g);
        return;
    }
    let n = g.shots.len();
    if let Some(d) = &g.diff {
        match ev.key {
            Key::Named(NamedKey::Escape) | Key::Char('q') => g.diff = None,
            Key::Char('!') if d.env_mismatch && d.result.is_none() => {
                start_diff(app, &mut g, true);
            }
            Key::Char('v') => fetch_image(app, &g),
            _ => {}
        }
        app.gallery.view = Some(g);
        return;
    }
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => {
            if g.view == View::Full && !g.pane_mode {
                g.view = View::List;
            } else {
                app.gallery.view = Some(g);
                close(app);
                return;
            }
        }
        Key::Char('j' | 'l') | Key::Named(NamedKey::Down | NamedKey::Right) => {
            g.sel = (g.sel + 1).min(n.saturating_sub(1));
            g.follow = false;
        }
        Key::Char('k' | 'h') | Key::Named(NamedKey::Up | NamedKey::Left) => {
            g.sel = g.sel.saturating_sub(1);
            if g.sel == 0 && g.pane_mode {
                g.follow = true;
            }
        }
        Key::Named(NamedKey::Enter) => g.view = View::Full,
        Key::Char('f') => {
            g.follow = !g.follow;
            if g.follow {
                g.sel = 0;
            }
        }
        Key::Char('r') => {
            app.gallery.view = Some(g);
            refresh(app);
            return;
        }
        Key::Char('v') => {
            if g.view == View::List {
                g.view = View::Full;
            }
            if crate::browser::gfx(app) == crate::browser::Gfx::Kitty {
                fetch_image(app, &g);
            } else {
                g.notice =
                    Some("This terminal shows no graphics — o opens the image locally".into());
            }
        }
        Key::Char('o') => open_locally(app, &g),
        Key::Char(' ') => {
            if let Some(s) = g.cur() {
                g.mark = if g.mark.as_deref() == Some(&s.id) {
                    None
                } else {
                    Some(s.id.clone())
                };
            }
        }
        Key::Char('d') => start_diff(app, &mut g, false),
        Key::Char('x') => {
            if let Some(s) = g.cur() {
                g.confirm_delete = Some(s.id.clone());
            }
        }
        Key::Char('t') => {
            g.scope = match (&g.scope, g.cur().and_then(|s| s.task.clone())) {
                (Scope::All, Some(t)) => Scope::Task(t),
                (Scope::All, None) => {
                    g.notice = Some("This screenshot isn't tied to a task".into());
                    Scope::All
                }
                _ => Scope::All,
            };
            g.sel = 0;
            app.gallery.view = Some(g);
            refresh(app);
            return;
        }
        Key::Char('L') => {
            let pv = g.cur().and_then(|s| s.preview.clone()).and_then(|h| {
                app.machines[g.machine]
                    .model
                    .previews
                    .iter()
                    .find(|p| p.handle == h || p.id == h)
                    .cloned()
            });
            match pv {
                Some(p) => {
                    let mi = g.machine;
                    let src = p.pane.clone();
                    app.mode = Mode::Normal;
                    crate::browser::open_preview(app, mi, &p, src);
                    return; // the gallery closes; the browser pane opens
                }
                None => g.notice = Some("No live preview for this screenshot".into()),
            }
        }
        Key::Char('y') => {
            if let Some(s) = g.cur() {
                let txt = format!("{} {}", s.handle, s.path);
                app.set_clipboard(txt.as_bytes(), false);
            }
        }
        _ => {}
    }
    app.gallery.view = Some(g);
    ensure_local_image(app);
}

// ---- replies ---------------------------------------------------------------------------------------------

fn view_mut(app: &mut App, id: u64) -> Option<&mut Gallery> {
    app.gallery.view.as_mut().filter(|g| g.id == id)
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    match r {
        Reply::Thumb { machine, preview } => {
            crate::preview_ui::on_reply(app, machine, preview, res)
        }
        Reply::List { view } => {
            let Some(g) = view_mut(app, view) else { return };
            g.loading = false;
            match res {
                Ok(x) => {
                    let keep = g.cur().map(|s| s.id.clone());
                    let mut shots: Vec<Shot> = arr(&x, "screenshots")
                        .iter()
                        .filter_map(Shot::parse)
                        .collect();
                    if let Scope::Pane(p) = &g.scope {
                        shots.retain(|s| s.pane.as_deref() == Some(p.as_str()));
                    }
                    g.shots = shots;
                    g.error = None;
                    g.sel = if g.follow {
                        0
                    } else {
                        keep.and_then(|k| g.shots.iter().position(|s| s.id == k))
                            .unwrap_or(g.sel)
                            .min(g.shots.len().saturating_sub(1))
                    };
                }
                Err(e) if e.is_method_not_found() => {
                    g.error = Some("This machine's server has no screenshot records".into())
                }
                Err(e) => g.error = Some(e.message),
            }
            ensure_local_image(app);
        }
        Reply::Image { view, key } => {
            app.gallery.fetching.remove(&key);
            let data = res
                .as_ref()
                .ok()
                .and_then(|x| x.get("data_b64").and_then(Value::as_str))
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok());
            let skipped = res
                .as_ref()
                .ok()
                .and_then(|x| x.get("inline_skipped").and_then(Value::as_str))
                .map(str::to_string);
            match data {
                Some(d) if d.len() <= MAX_IMAGE => app.gallery.put_image(key, d),
                _ => {
                    if let Some(g) = view_mut(app, view) {
                        g.notice = Some(match (res, skipped) {
                            (Err(e), _) => format!("✗ {}", e.message),
                            (_, Some(s)) => format!("image not transferred: {s} — o opens it"),
                            _ => "image not available".into(),
                        });
                    }
                }
            }
        }
        Reply::Diff { view } => {
            let Some(g) = view_mut(app, view) else { return };
            let Some(d) = &mut g.diff else { return };
            match res {
                Ok(x) => {
                    d.result = Some(x);
                    d.error = None;
                }
                Err(e) if e.reason() == Some("environment_mismatch") => {
                    d.env_mismatch = true;
                    d.error = Some(format!(
                        "Different environments ({} vs {}) — the images may not be comparable",
                        d.a.label, d.b.label
                    ));
                }
                Err(e) => d.error = Some(e.message),
            }
            ensure_local_image(app);
        }
        Reply::Deleted { view } => {
            let Some(g) = view_mut(app, view) else { return };
            match res {
                Ok(_) => g.notice = Some("Screenshot deleted".into()),
                Err(e) if e.reason() == Some("referenced_by_acceptance") => {
                    g.force_delete = g.cur().map(|s| s.id.clone());
                }
                Err(e) => g.notice = Some(format!("✗ {}", e.message)),
            }
            refresh(app);
        }
        Reply::Opened => match res {
            Ok(x) => {
                let data = x
                    .get("data_b64")
                    .and_then(Value::as_str)
                    .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok());
                let id = st(&x, "id").replace(['/', '\\', '.'], "_");
                match data {
                    Some(d) => {
                        let dir = std::env::temp_dir().join("vibeke-screenshots");
                        let path = dir.join(format!("{id}.png"));
                        let ok = std::fs::create_dir_all(&dir).is_ok()
                            && std::fs::write(&path, d).is_ok();
                        if ok {
                            os_open(app, &path.to_string_lossy());
                        } else {
                            app.toast("✗ couldn't write the image to a temp file");
                        }
                    }
                    None => app.toast(format!(
                        "✗ {} sent no image",
                        app.machines.get(mi).map(|m| m.label.as_str()).unwrap_or("")
                    )),
                }
            }
            Err(e) => app.toast(format!("✗ {}", e.message)),
        },
    }
}

// ---- kitty transmission (before drawing) --------------------------------------------------------------------

/// Cells for an image of `w × h` px fitted into `max_c × max_r` cells (aspect kept).
pub fn fit_cells(app: &App, w: u32, h: u32, max_c: u16, max_r: u16) -> (u16, u16) {
    if w == 0 || h == 0 || max_c == 0 || max_r == 0 {
        return (0, 0);
    }
    let (cw, ch, _) = crate::browser::cell_geom(app);
    let sx = (max_c as f64 * cw as f64) / w as f64;
    let sy = (max_r as f64 * ch as f64) / h as f64;
    let s = sx.min(sy);
    let c = ((w as f64 * s) / cw as f64).floor().max(1.0) as u16;
    let r = ((h as f64 * s) / ch as f64).floor().max(1.0) as u16;
    (c.min(max_c), r.min(max_r))
}

/// PNG dimensions from the IHDR chunk.
pub fn png_size(d: &[u8]) -> Option<(u32, u32)> {
    if d.len() < 24 || &d[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes(d[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(d[20..24].try_into().ok()?);
    Some((w, h))
}

/// Where the image goes this frame: (key, x, y, cols, rows), if one is shown.
pub fn placement(app: &App) -> Option<(String, u16, u16, u16, u16)> {
    if crate::browser::gfx(app) != crate::browser::Gfx::Kitty
        || !matches!(app.mode, Mode::Popup(Popup::Gallery))
    {
        return None;
    }
    let g = app.gallery.view.as_ref()?;
    let (key, header) = match &g.diff {
        Some(d) => (
            image_key(
                g.machine,
                &format!("diff:{}", st(d.result.as_ref()?, "blob")),
            ),
            8u16,
        ),
        None if g.view == View::Full => (image_key(g.machine, &g.cur()?.id), 5u16),
        None => return None,
    };
    let data = app.gallery.image(&key)?;
    let (w, h) = png_size(data)?;
    let a = app.pane_area();
    let top = a.y + 1 + header + u16::from(g.notice.is_some());
    let max_r = (a.y + a.h).saturating_sub(top + 1);
    let max_c = a.w.saturating_sub(2);
    let (c, r) = fit_cells(app, w, h, max_c, max_r);
    if c == 0 || r == 0 {
        return None;
    }
    Some((key, a.x + 1, top, c, r))
}

/// Transmit a PNG for `h`. When the host terminal is on this machine (not behind SSH) the PNG
/// goes through a temp file the terminal reads and deletes (`t=t`, 06 B8) instead of base64
/// through the pty; otherwise (or when no private temp file can be made, or
/// `VIBEKE_GFX_TRANSFER=direct`) it is chunked `t=d`.
pub fn transmit_png(app: &mut App, out: &mut Vec<u8>, h: &vk_browser::kitty::Header, data: &[u8]) {
    if !app.caps.host_remote
        && std::env::var("VIBEKE_GFX_TRANSFER").as_deref() != Ok("direct")
        && let Some(dir) = private_gfx_dir(app.gallery.gfx_dir.as_deref())
        && let Ok(path) = vk_browser::kitty::write_temp_file(&dir, data)
    {
        vk_browser::kitty::transmit_temp_file(out, h, &path, data.len());
        return;
    }
    vk_browser::kitty::transmit_direct(out, h, data);
}

/// A directory only this user can use (0700, ours, not a symlink); stale files from terminals
/// that never read them are removed.
fn private_gfx_dir(custom: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let dir = custom
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("vibeke-gfx-{uid}")));
    if std::fs::symlink_metadata(&dir).is_err() {
        let _ = std::fs::DirBuilder::new().mode(0o700).create(&dir);
    }
    let md = std::fs::symlink_metadata(&dir).ok()?;
    if !md.is_dir() || md.uid() != uid || md.mode() & 0o077 != 0 {
        return None;
    }
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let old = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > std::time::Duration::from_secs(120));
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Some(dir)
}

/// Transmit or delete the gallery image so the next frame's placeholders resolve. Called from
/// `App::draw` before composing (the only mutable point of a frame).
pub fn before_draw(app: &mut App) {
    let want = placement(app);
    if want == app.gallery.sent {
        return;
    }
    let mut out = Vec::new();
    if app.gallery.sent.take().is_some() {
        vk_browser::kitty::delete_image(&mut out, IMAGE_ID);
    }
    if let Some((key, x, y, c, r)) = want
        && let Some(data) = app.gallery.image(&key)
    {
        let mut h =
            vk_browser::kitty::Header::new(IMAGE_ID, vk_browser::kitty::PixelFormat::Png, 0, 0);
        h.virtual_cells = Some((c, r));
        h.placement = Some(1);
        let data = data.to_vec();
        transmit_png(app, &mut out, &h, &data);
        app.gallery.sent = Some((key, x, y, c, r));
    }
    app.browser.out.extend_from_slice(&out);
}

// ---- drawing ---------------------------------------------------------------------------------------------

fn age(ms: i64) -> String {
    if ms <= 0 {
        return "—".into();
    }
    crate::inbox::fmt_age(now_ms() - ms)
}

fn scope_text(s: &Scope) -> String {
    match s {
        Scope::All => "all".into(),
        Scope::Task(t) => format!("task {}", truncate(t, 12)),
        Scope::Pane(_) => "this agent".into(),
    }
}

fn meta_lines(app: &App, s: &Shot, a: &mut Area) {
    let t = app.theme;
    a.line(
        &format!(
            "{} · {} · {}×{} · {} ago",
            s.handle,
            s.label,
            s.width,
            s.height,
            age(s.created_at_ms)
        ),
        t.bold(t.fg),
    );
    let bstyle = if s.binding == "bound" {
        t.s(t.green)
    } else {
        t.s(t.yellow)
    };
    a.line(&s.binding_text(), bstyle);
    a.line(&format!("code: {}", s.code_text()), t.text());
    if s.env_kind == "remote_headless" {
        a.line(
            "headless, fresh context — not proof of what your logged-in profile shows",
            t.dim(),
        );
    }
    a.line(
        &format!(
            "{}{}",
            s.url,
            if s.title.is_empty() {
                String::new()
            } else {
                format!(" — {}", s.title)
            }
        ),
        t.text(),
    );
}

pub fn draw(app: &App, g: &mut Grid) {
    let Some(gv) = &app.gallery.view else {
        return;
    };
    let t = app.theme;
    let title = if gv.pane_mode {
        format!(
            "screenshot pane · {}{}",
            scope_text(&gv.scope),
            if gv.follow { " · following new" } else { "" }
        )
    } else {
        format!(
            "screenshots · {} · {}",
            scope_text(&gv.scope),
            app.machines[gv.machine].label
        )
    };
    let mut a = Area::open(app, g, &title);
    if let Some(n) = &gv.notice {
        a.line(n, t.bold(t.yellow));
    }
    let kitty = crate::browser::gfx(app) == crate::browser::Gfx::Kitty;
    let local = app.machines[gv.machine].local;
    if let Some(d) = &gv.diff {
        a.line(
            &format!("diff {} → {}", d.a.handle, d.b.handle),
            t.bold(t.fg),
        );
        a.line(&format!("A: {} · {}", d.a.label, d.a.code_text()), t.text());
        a.line(&format!("B: {} · {}", d.b.label, d.b.code_text()), t.text());
        match (&d.result, &d.error) {
            (Some(r), _) => {
                let ratio = r
                    .get("changed_ratio")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                a.line(
                    &format!(
                        "{:.2}% of pixels changed · {} region(s){}",
                        ratio * 100.0,
                        r.get("regions_total")
                            .and_then(Value::as_u64)
                            .unwrap_or(arr(r, "regions").len() as u64),
                        if r.get("size_mismatch").and_then(Value::as_bool) == Some(true) {
                            " · sizes differ"
                        } else {
                            ""
                        }
                    ),
                    t.s(if ratio > 0.0 { t.yellow } else { t.green }),
                );
                for reg in arr(r, "regions").iter().take(3) {
                    a.line(
                        &format!(
                            "  region {}×{} at {},{}",
                            reg["width"], reg["height"], reg["x"], reg["y"]
                        ),
                        t.dim(),
                    );
                }
                draw_image_or_hint(app, &mut a, kitty, local, true);
            }
            (None, Some(e)) => {
                a.line(e, t.s(t.red));
                if d.env_mismatch {
                    a.footer("[!] compare anyway   esc back", t.bold(t.yellow));
                    return;
                }
            }
            (None, None) => a.line("comparing…", t.dim()),
        }
        a.footer("v show diff image · esc back", t.dim());
        return;
    }
    if let Some(e) = &gv.error {
        a.line(e, t.s(t.red));
    }
    if gv.shots.is_empty() {
        a.line(
            if gv.loading {
                "loading…"
            } else {
                "No screenshots yet — agents' browser_screenshot and the browser pane's prefix+shift+s add them"
            },
            t.dim(),
        );
    }
    match gv.view {
        View::Full => {
            if let Some(s) = gv.cur() {
                meta_lines(app, s, &mut a);
                draw_image_or_hint(app, &mut a, kitty, local, false);
            }
            a.footer(
                &format!(
                    "{}/{} · ←/→ history · d diff · space mark · v image · o open locally · L live preview · f follow · x delete · esc {}",
                    gv.sel + 1,
                    gv.shots.len(),
                    if gv.pane_mode { "close" } else { "list" }
                ),
                t.dim(),
            );
        }
        View::List => {
            let max = a.left().saturating_sub(5) as usize;
            let skip = gv.sel.saturating_sub(max.saturating_sub(1));
            for (i, s) in gv.shots.iter().enumerate().skip(skip).take(max) {
                let mark = if gv.mark.as_deref() == Some(&s.id) {
                    "◆"
                } else {
                    " "
                };
                let bind = if s.binding == "bound" { "✓" } else { "~" };
                let row = format!(
                    "{mark} {:<5} {:>4} {bind} {:<38} {}",
                    s.handle,
                    age(s.created_at_ms),
                    truncate(&s.label, 38),
                    s.url
                );
                a.line(&row, if i == gv.sel { t.sel(t.fg) } else { t.text() });
            }
            if let Some(s) = gv.cur() {
                a.line("", t.text());
                meta_lines(app, s, &mut a);
            }
            if gv.confirm_delete.is_some() {
                a.footer(
                    "Delete this screenshot? [y] delete  [any key] keep",
                    t.bold(t.yellow),
                );
            } else if gv.force_delete.is_some() {
                a.footer(
                    "Kept for a review acceptance — [!] delete anyway  [any key] keep",
                    t.bold(t.yellow),
                );
            } else {
                a.footer(
                    "j/k · enter view · d diff (space marks base) · o open locally · t scope · L live preview · x delete · y copy · esc",
                    t.dim(),
                );
            }
        }
    }
}

fn draw_image_or_hint(app: &App, a: &mut Area, kitty: bool, local: bool, diff: bool) {
    let t = app.theme;
    if !kitty {
        a.line(
            "This terminal shows no graphics — [o] open image locally",
            t.dim(),
        );
        return;
    }
    match placement(app) {
        Some((_, x, y, c, r)) => {
            for row in 0..r {
                for col in 0..c {
                    a.g.put_grapheme(
                        x + col,
                        y + row,
                        &crate::browser::placeholder(IMAGE_ID, row, col),
                        crate::browser::id_style(IMAGE_ID),
                    );
                }
            }
            a.y = a.y.max(y + r);
        }
        None if local => a.line("loading image…", t.dim()),
        None => a.line(
            if diff {
                "[v] fetch the diff image from the remote machine"
            } else {
                "[v] fetch the image from the remote machine (not transferred until you ask)"
            },
            t.dim(),
        ),
    }
}

#[cfg(test)]
#[path = "gallery_tests.rs"]
mod tests;
