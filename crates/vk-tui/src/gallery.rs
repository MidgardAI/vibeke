//! Screenshot gallery and screenshot pane (06 B8; server: `screenshot.list/get/open/delete`,
//! `browser.diff`, event `screenshot.captured`).
//!
//! - **Gallery** (`:screenshots`, agent peek `p` for that agent's screenshots): a full pane-area
//!   view, newest first. Each row shows the handle, age, environment label ("devbox · headless ·
//!   fresh context" vs "your browser pane · profile devbox"), binding (`bound` / `illustrative`
//!   with the reason), code state and URL. `enter` shows the selected screenshot full size.
//! - **Screenshot pane** (`:screenshot_pane`): the same view in full-image mode that follows new
//!   screenshots as they are captured (`f` toggles following).
//! - **Images**: drawn through [`crate::image_view`] (kitty graphics or iTerm2 inline images).
//!   Screenshots on the local machine are read from disk; a remote machine's image is fetched
//!   at the size of the view only on an explicit `v` (never sent over the link unasked).
//!   Without graphics: metadata and `o` **open image locally** (the OS opener, only on that
//!   explicit key; a download in the WASM TUI).
//! - **Diff**: `space` marks a base, `d` diffs it (or the next older screenshot) against the
//!   selected one with `browser.diff`; different environments are refused unless `!` forces.
//! - `x` delete (confirm; a screenshot kept for a review acceptance needs `!`), `t` scope
//!   (all / this task), `L` open the live preview in a browser pane, `y` copy handle + path.
//! - **📷 counters**: sidebar agent rows and unfocused pane corners show `📷N`, the pane's unseen
//!   images ([`crate::images`]). Showing an image in the full view counts as seeing it; opening
//!   the list does not.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::drafts::{Area, arr, now_ms, st};
use crate::draw::truncate;
use crate::screen::Grid;
use base64::Engine as _;
use serde_json::{Value, json};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

use crate::image_view::{self as iv, Placement};
pub use crate::image_view::{fit_cells, png_size, transmit_png};
use crate::screen::Rect;

/// Largest original `screenshot.open` carries from a remote machine (the server's inline
/// limit).
pub const MAX_OPEN: u64 = 8 << 20;

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
    /// The image of a remote sidebar thumbnail (`screenshot.get {inline, thumb}`).
    ThumbImage {
        key: String,
    },
    /// The recent images of a machine (sidebar section, viewer), [`crate::images`].
    Recent {
        seq: u64,
    },
    /// The viewer's image.
    Viewer {
        key: String,
    },
    /// `screenshot.delete` from the viewer or a sidebar row.
    Removed {
        id: String,
    },
    /// The attach pop-up's image ([`crate::card`]).
    Card {
        key: String,
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
    pub caption: Option<String>,
    pub source_name: Option<String>,
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

/// Screenshot metadata is shown as text and can carry what the page supplied (title, final
/// URL, the build id inside `binding_reason`): control characters become visible escapes.
fn clean(s: &str) -> String {
    vk_proto::text::escape_controls(s).into_owned()
}

fn opt(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(clean)
}

/// The first `n` characters (never splits one).
fn prefix(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

impl Shot {
    pub fn parse(v: &Value) -> Option<Shot> {
        let id = v
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())?
            .to_string();
        let code = v.get("code").cloned().unwrap_or(Value::Null);
        let text = |k: &str| clean(st(v, k));
        Some(Shot {
            id,
            handle: text("handle"),
            label: text("label"),
            url: opt(v, "final_url").unwrap_or_else(|| text("url")),
            title: text("title"),
            created_at_ms: v.get("created_at_ms").and_then(Value::as_i64).unwrap_or(0),
            binding: text("binding"),
            binding_reason: text("binding_reason"),
            env_kind: clean(
                v.pointer("/environment/kind")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            ),
            caption: opt(v, "caption"),
            source_name: opt(v, "source_name"),
            head_sha: opt(&code, "head_sha"),
            dirty_state: opt(&code, "dirty_state"),
            task: opt(v, "task"),
            pane: opt(v, "pane").or_else(|| {
                v.pointer("/taken_by/pane")
                    .and_then(Value::as_str)
                    .map(clean)
            }),
            preview: opt(v, "preview"),
            path: text("path_on_machine"),
            exists: v.get("exists").and_then(Value::as_bool).unwrap_or(true),
            width: v.get("width").and_then(Value::as_u64).unwrap_or(0) as u32,
            height: v.get("height").and_then(Value::as_u64).unwrap_or(0) as u32,
            bytes: v.get("bytes").and_then(Value::as_u64).unwrap_or(0),
        })
    }

    pub fn is_agent(&self) -> bool {
        self.env_kind == "agent"
    }

    /// What a row and the full view show instead of a URL for an image an agent attached.
    pub fn agent_text(&self) -> String {
        self.caption
            .clone()
            .or_else(|| self.source_name.clone())
            .unwrap_or_else(|| "attached image".into())
    }

    /// The environment label; an attached image has no browser environment.
    pub fn label_text(&self) -> &str {
        if self.is_agent() {
            "attached by agent"
        } else {
            &self.label
        }
    }

    pub fn code_text(&self) -> String {
        match (&self.head_sha, self.dirty_state.as_deref()) {
            (Some(h), Some("dirty")) => format!("{} + uncommitted changes", prefix(h, 8)),
            (Some(h), Some("unknown")) => format!("{} (dirty state unknown)", prefix(h, 8)),
            (Some(h), _) => prefix(h, 8).to_string(),
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
    /// Select this screenshot when the list arrives (opened from the viewer).
    pub want_sel: Option<String>,
}

impl Gallery {
    pub fn cur(&self) -> Option<&Shot> {
        self.shots.get(self.sel)
    }
}

/// Client state: the open view (images are in [`crate::image_view`], 📷 counters in
/// [`crate::images`]).
#[derive(Debug, Default)]
pub struct GalleryState {
    pub view: Option<Gallery>,
    /// Files handed to the OS opener (tests read this instead of spawning).
    pub opened: Vec<String>,
    #[cfg(target_arch = "wasm32")]
    pub downloads: Vec<(String, Vec<u8>)>,
}

// ---- counters ----------------------------------------------------------------------------------------

/// The 📷 counter for a pane (sidebar rows, pane corners): its unseen images.
pub fn badge(app: &App, mi: usize, pane: &str) -> Option<u32> {
    Some(crate::images::unseen_count(app, mi, pane)).filter(|n| *n > 0)
}

/// A pushed `screenshot.captured` (the unseen state is [`crate::images::on_captured`]).
pub fn on_captured(app: &mut App, mi: usize, _v: &Value) {
    if app.gallery.view.as_ref().is_some_and(|g| g.machine == mi) {
        refresh(app);
    }
}

/// A pushed `screenshot.deleted` (`data.ids`): a pending delete of one of them is off.
pub fn on_deleted(app: &mut App, mi: usize, ids: &[String]) {
    let Some(g) = app.gallery.view.as_mut().filter(|g| g.machine == mi) else {
        return;
    };
    let gone = |x: &Option<String>| x.as_ref().is_some_and(|id| ids.contains(id));
    if gone(&g.confirm_delete) {
        g.confirm_delete = None;
    }
    if gone(&g.force_delete) {
        g.force_delete = None;
    }
    refresh(app);
}

/// The full view shows an image: it counts as seen (spec 18 §4.2).
fn mark_shown_seen(app: &mut App) {
    if !matches!(app.mode, Mode::Popup(Popup::Gallery)) {
        return;
    }
    let shown = app
        .gallery
        .view
        .as_ref()
        .filter(|g| g.view == View::Full && g.diff.is_none())
        .and_then(|g| g.cur().map(|s| (g.machine, s.id.clone())));
    if let Some((mi, id)) = shown {
        crate::images::mark_seen(app, mi, &id);
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
        want_sel: None,
    });
    app.mode = Mode::Popup(Popup::Gallery);
    refresh(app);
}

/// Machine `mi`'s gallery with screenshot `id` selected once the list arrives (viewer `g`).
pub fn open_on(app: &mut App, mi: usize, id: &str) {
    open(app, mi, Scope::All, View::List, false);
    if let Some(g) = &mut app.gallery.view {
        g.want_sel = Some(id.to_string());
    }
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

fn diff_key(mi: usize, blob: &str) -> String {
    format!("{mi}:diff:{blob}")
}

/// What the gallery shows an image for this frame: (slot, key, area it may fill, the shot when
/// it is a screenshot rather than a diff).
fn wanted<'a>(app: &App, g: &'a Gallery) -> Option<(&'static str, String, Rect, Option<&'a Shot>)> {
    let a = app.pane_area();
    let (slot, key, header, shot) = match &g.diff {
        Some(d) => (
            "diff",
            diff_key(g.machine, st(d.result.as_ref()?, "blob")),
            8u16,
            None,
        ),
        None if g.view == View::Full => {
            let s = g.cur()?;
            let max_c = a.w.saturating_sub(2);
            let k = iv::key(app, g.machine, &s.id, iv::thumb_px(app, max_c));
            ("gallery", k, 5u16, Some(s))
        }
        None => return None,
    };
    let top = a.y + 1 + header + u16::from(g.notice.is_some());
    let area = Rect {
        x: a.x + 1,
        y: top,
        w: a.w.saturating_sub(2),
        h: (a.y + a.h).saturating_sub(top + 1),
    };
    Some((slot, key, area, shot))
}

/// Load the selected (or diff) image when that needs no transfer: local machines read the PNG
/// from disk. Remote images wait for an explicit `v`.
pub fn ensure_local_image(app: &mut App) {
    if iv::gfx(app) == crate::browser::Gfx::None {
        return;
    }
    let Some(g) = &app.gallery.view else {
        return;
    };
    if !iv::readable(app, g.machine) {
        return;
    }
    let want = match &g.diff {
        Some(d) => d.result.as_ref().map(|r| {
            (
                diff_key(g.machine, st(r, "blob")),
                st(r, "path_on_machine").to_string(),
                true,
            )
        }),
        None => wanted(app, g).and_then(|(_, k, _, s)| s.map(|s| (k, s.path.clone(), s.exists))),
    };
    if let Some((key, path, exists)) = want {
        iv::load_local(app, &key, &path, exists);
    }
}

/// `v`: fetch the selected image from its machine (explicit for remote machines), at the size
/// of the view.
fn fetch_image(app: &mut App, g: &Gallery) {
    if let Some(d) = &g.diff {
        let Some(r) = &d.result else { return };
        let key = diff_key(g.machine, st(r, "blob"));
        if app.imgview.image(&key).is_none() && app.imgview.fetching.insert(key.clone()) {
            app.command_on(
                g.machine,
                "browser.diff",
                json!({"a": d.a.id, "b": d.b.id, "inline": true, "force": d.env_mismatch}),
                Pending::Gallery(Reply::Image { view: g.id, key }),
            );
        }
        return;
    }
    let Some((_, key, area, Some(s))) = wanted(app, g) else {
        return;
    };
    let px = iv::thumb_px(app, area.w);
    let id = s.id.clone();
    // An explicit `v` tries again after a failed transfer.
    app.imgview.problems.remove(&key);
    iv::fetch(
        app,
        g.machine,
        &id,
        px,
        &key,
        Pending::Gallery(Reply::Image {
            view: g.id,
            key: key.clone(),
        }),
    );
}

/// The action that still works when no image is shown.
pub fn open_label() -> &'static str {
    if cfg!(target_arch = "wasm32") {
        "o download"
    } else {
        "o open locally"
    }
}

/// A remote original too large for `screenshot.open` (§3.4): what to say instead of `o`.
pub fn too_large_to_open(app: &App, mi: usize, s: &Shot) -> Option<String> {
    (!iv::readable(app, mi) && s.bytes > MAX_OPEN).then(|| {
        format!(
            "{} is too large to transfer ({} MiB) — on {}: {}",
            s.handle,
            s.bytes >> 20,
            app.machines[mi].label,
            s.path
        )
    })
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
/// (`screenshot.open`, explicit) into `$TMPDIR/vibeke-screenshots/<id>.png` first (a download
/// in the WASM TUI).
fn open_locally(app: &mut App, g: &mut Gallery) {
    let Some(s) = g.cur().cloned() else { return };
    if let Some(msg) = open_shot(app, g.machine, &s) {
        g.notice = Some(msg);
    }
}

/// Open screenshot `s` of machine `mi` on this machine (gallery and viewer `o`). Returns the
/// notice to show instead when a remote original is too large to transfer.
pub(crate) fn open_shot(app: &mut App, mi: usize, s: &Shot) -> Option<String> {
    if let Some(msg) = too_large_to_open(app, mi, s) {
        return Some(msg);
    }
    if iv::readable(app, mi) {
        if s.exists {
            os_open(app, &s.path);
        } else {
            app.toast("the image file is gone (retention)");
        }
        return None;
    }
    app.command_on(
        mi,
        "screenshot.open",
        json!({"id": s.id}),
        Pending::Gallery(Reply::Opened),
    );
    app.toast(format!(
        "fetching {} from {}…",
        s.handle, app.machines[mi].label
    ));
    None
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
            if iv::gfx(app) != crate::browser::Gfx::None {
                fetch_image(app, &g);
            } else {
                g.notice = iv::hint(app, "", open_label());
            }
        }
        Key::Char('o') => open_locally(app, &mut g),
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
    mark_shown_seen(app);
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
        Reply::ThumbImage { key } => crate::preview_ui::on_image(app, &key, &res),
        Reply::Recent { seq } => crate::images::on_recent(app, mi, seq, res),
        Reply::Viewer { key } => {
            iv::on_fetched(app, &key, &res);
        }
        Reply::Removed { id } => crate::images::on_removed(app, mi, id, res),
        Reply::Card { key } => crate::card::on_fetched(app, &key, &res),
        Reply::List { view } => {
            let Some(g) = view_mut(app, view) else { return };
            g.loading = false;
            match res {
                Ok(x) => {
                    let want = g.want_sel.take();
                    if want.is_some() {
                        g.follow = false;
                    }
                    let keep = want.or_else(|| g.cur().map(|s| s.id.clone()));
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
            mark_shown_seen(app);
        }
        Reply::Image { view, key } => {
            if iv::on_fetched(app, &key, &res).is_some() {
                let hint = iv::hint(app, &key, open_label());
                if let Some(g) = view_mut(app, view) {
                    g.notice = hint;
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
                    #[cfg(target_arch = "wasm32")]
                    Some(d) => {
                        app.gallery.downloads.push((format!("{id}.png"), d));
                        app.toast("Screenshot ready to download");
                    }
                    #[cfg(not(target_arch = "wasm32"))]
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

// ---- placement -------------------------------------------------------------------------------------

/// The gallery's image placement this frame (full view or diff), if its image is loaded.
pub fn placements(app: &App) -> Option<Placement> {
    if !matches!(app.mode, Mode::Popup(Popup::Gallery)) {
        return None;
    }
    let g = app.gallery.view.as_ref()?;
    let (slot, key, area, _) = wanted(app, g)?;
    iv::fit(app, slot, &key, area)
}

// ---- drawing ---------------------------------------------------------------------------------------------

pub(crate) fn age(ms: i64) -> String {
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
            s.label_text(),
            s.width,
            s.height,
            age(s.created_at_ms)
        ),
        t.bold(t.fg),
    );
    if s.is_agent() {
        a.line(&s.agent_text(), t.text());
        return;
    }
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
    let local = iv::readable(app, gv.machine);
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
                draw_image_or_hint(app, &mut a, local, true);
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
                draw_image_or_hint(app, &mut a, local, false);
            }
            let open = match gv.cur() {
                Some(s) if too_large_to_open(app, gv.machine, s).is_some() => String::new(),
                _ => format!(" · {}", open_label()),
            };
            a.footer(
                &format!(
                    "{}/{} · ←/→ history · d diff · space mark · v image{open} · L live preview · f follow · x delete · esc {}",
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
                let last = if s.is_agent() {
                    s.agent_text()
                } else {
                    s.url.clone()
                };
                let bind = if s.is_agent() { " " } else { bind };
                let row = format!(
                    "{mark} {:<5} {:>4} {bind} {:<38} {}",
                    s.handle,
                    age(s.created_at_ms),
                    truncate(s.label_text(), 38),
                    last
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
                    &format!(
                        "j/k · enter view · d diff (space marks base) · {} · t scope · L live preview · x delete · y copy · esc",
                        open_label()
                    ),
                    t.dim(),
                );
            }
        }
    }
}

fn draw_image_or_hint(app: &App, a: &mut Area, local: bool, diff: bool) {
    let t = app.theme;
    let Some(gv) = &app.gallery.view else { return };
    let key = wanted(app, gv).map(|(_, k, _, _)| k).unwrap_or_default();
    let open = if cfg!(target_arch = "wasm32") {
        "[o] download the image"
    } else {
        "[o] open image locally"
    };
    if let Some(h) = iv::hint(app, &key, open) {
        a.line(&h, t.dim());
        return;
    }
    match placements(app) {
        Some(p) if iv::draw(app, a.g, &p) => a.y = a.y.max(p.rect.y + p.rect.h),
        Some(_) => a.line("loading image…", t.dim()),
        None if local || app.imgview.fetching.contains(&key) => a.line("loading image…", t.dim()),
        None => a.line(
            if diff {
                "[v] fetch the diff image from the remote machine"
            } else {
                "[v] view image — fetched from the remote machine only when you ask"
            },
            t.dim(),
        ),
    }
}

#[cfg(test)]
#[path = "gallery_tests.rs"]
mod tests;
