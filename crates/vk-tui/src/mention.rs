//! Mentions in the TUI (spec 19 §5): the `prefix+@` picker, the palette actions that name a pane
//! and the `@name` marks in the sidebar and tab bar.
//!
//! The picker lists the other panes of the focused pane's workspace. **Enter** inserts `@name `
//! into the focused pane as a paste (naming the target first when it has no name), **Tab** does
//! the same and stays open, **ctrl+e** edits the shown name and **esc** closes. Names are
//! written with `mention.set`; the server owns the rules (`name_taken`), the picker shows its
//! error and stays open.

use crate::app::{App, Mode, Pending, Popup, PromptKind, RpcErr};
use crate::nav::{ListKey, fuzzy, list_key, list_row};
use crate::screen::{Grid, Rect as SRect};
use serde_json::{Value, json};
use std::collections::HashSet;
use unicode_width::UnicodeWidthStr;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::mention::{self, MAX_LEN};
use vk_proto::model::{Execution, InteractionStatus};

/// Widest the picker gets, in columns.
const WIDTH: u16 = 64;

/// What the picker remembers between keys.
#[derive(Debug, Clone, Default)]
pub struct Picker {
    pub filter: String,
    pub sel: usize,
    /// Names inserted so far with Tab (shown in the footer).
    pub inserted: Vec<String>,
    /// The name being edited (ctrl+e).
    pub edit: Option<Edit>,
    /// Names the user edited, by pane id. They replace the suggestion (or the current name).
    pub overrides: Vec<(String, String)>,
    /// The last failure, shown in the footer until the next key.
    pub error: Option<String>,
    /// A `mention.set` is in flight.
    pub busy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub pane: String,
    pub input: String,
}

/// One row of the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub pane: String,
    /// The pane's current mention name, if it has one.
    pub name: Option<String>,
    /// The name the row uses: an edit, the current name or the suggestion (without `@`).
    pub shown: String,
    /// The sidebar label (`draw::pane_title`).
    pub label: String,
    /// `idle`, `busy`, `needs input`, `exited`, `error`, `limited` or `shell`.
    pub state: &'static str,
    /// The harness of an agent pane, else empty.
    pub harness: String,
    /// The pane runs no agent.
    pub terminal: bool,
}

impl Entry {
    /// Using this row needs a `mention.set` first.
    pub fn needs_set(&self) -> bool {
        self.name.as_deref() != Some(self.shown.as_str())
    }
}

/// The reply a `mention.set` from the picker waits for.
#[derive(Debug, Clone)]
pub enum Reply {
    Set {
        /// The pane that receives the `@name `.
        target: String,
        name: String,
        keep_open: bool,
    },
}

// ---- model -------------------------------------------------------------------------------------

fn state_of(m: &crate::app::Machine, pane: &str, run_exec: Option<&Execution>) -> &'static str {
    let Some(exec) = run_exec else {
        return "shell";
    };
    let open = m
        .model
        .interactions
        .iter()
        .any(|i| i.pane == pane && i.status == InteractionStatus::Open);
    if open && *exec != Execution::Exited {
        return "needs input";
    }
    match exec {
        Execution::Idle => "idle",
        Execution::Starting | Execution::Working | Execution::Unknown => "busy",
        Execution::Exited => "exited",
        Execution::Error => "error",
        Execution::RateLimited => "limited",
    }
}

/// The run that stands for a pane: the newest one.
fn run_of<'a>(m: &'a crate::app::Machine, pane: &str) -> Option<&'a vk_proto::model::AgentRun> {
    m.model
        .runs
        .iter()
        .filter(|r| r.pane == pane)
        .max_by_key(|r| r.started_at_ms)
}

/// Base name of a working directory (the suggestion for a shell pane).
fn cwd_base(cwd: Option<&str>) -> String {
    cwd.map(|c| c.trim_end_matches('/'))
        .and_then(|c| c.rsplit('/').next())
        .unwrap_or_default()
        .to_string()
}

/// The panes the picker offers, in model order, with the name each row would use. Suggestions
/// never repeat a name of the workspace, an edit or an earlier row.
pub fn entries(app: &App, overrides: &[(String, String)]) -> Vec<Entry> {
    let m = app.m();
    let Some(fp) = app.focused_pane() else {
        return vec![];
    };
    let Some(focused) = m.model.panes.iter().find(|p| p.id == fp) else {
        return vec![];
    };
    let ws = focused.workspace.clone();
    let mut taken: HashSet<String> = m
        .model
        .mentions
        .iter()
        .filter(|x| x.workspace == ws)
        .map(|x| x.name.clone())
        .collect();
    taken.extend(overrides.iter().map(|(_, n)| n.clone()));
    let mut out = Vec::new();
    for p in m.model.panes.iter().filter(|p| {
        p.workspace == ws && p.id != fp && !p.is_browser() && p.plugin_surface().is_none()
    }) {
        let run = run_of(m, &p.id);
        let label = crate::draw::pane_title(m, &p.id);
        let name = m.model.mention_of(&p.id).map(|x| x.name.clone());
        let over = overrides
            .iter()
            .find(|(id, _)| *id == p.id)
            .map(|(_, n)| n.clone());
        let shown = match (over, &name) {
            (Some(o), _) => o,
            (None, Some(n)) => n.clone(),
            (None, None) => {
                let fallback = match run {
                    Some(r) => r.harness.clone(),
                    None => cwd_base(p.cwd.as_deref()),
                };
                let s = mention::suggest(&label, &fallback, |n| taken.contains(n));
                taken.insert(s.clone());
                s
            }
        };
        out.push(Entry {
            pane: p.id.clone(),
            name,
            shown,
            label,
            state: state_of(m, &p.id, run.map(|r| &r.execution.value)),
            harness: run.map(|r| r.harness.clone()).unwrap_or_default(),
            terminal: run.is_none(),
        });
    }
    out
}

/// Entries matching the filter (fuzzy over name, label and harness), best first.
pub fn ranked(app: &App, p: &Picker) -> Vec<Entry> {
    let mut v: Vec<(i32, usize, Entry)> = entries(app, &p.overrides)
        .into_iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let text = format!("{} {} {}", e.shown, e.label, e.harness);
            let m = fuzzy(&p.filter, &text)?;
            Some((m.score, i, e))
        })
        .collect();
    if !p.filter.trim().is_empty() {
        v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    }
    v.into_iter().map(|(_, _, e)| e).collect()
}

/// The focused pane has a mention name.
pub fn focused_named(app: &App) -> bool {
    app.focused_pane()
        .is_some_and(|p| app.m().model.mention_of(&p).is_some())
}

/// The mention name of `pane`, for labels.
pub fn name_of<'a>(m: &'a crate::app::Machine, pane: &str) -> Option<&'a str> {
    m.model.mention_of(pane).map(|x| x.name.as_str())
}

/// A name for `pane`: its current one, else a suggestion that is free in its workspace.
fn name_or_suggestion(app: &App, pane: &str) -> String {
    let m = app.m();
    if let Some(n) = name_of(m, pane) {
        return n.to_string();
    }
    let ws = m
        .model
        .panes
        .iter()
        .find(|p| p.id == pane)
        .map(|p| p.workspace.clone())
        .unwrap_or_default();
    let label = crate::draw::pane_title(m, pane);
    let fallback = match run_of(m, pane) {
        Some(r) => r.harness.clone(),
        None => cwd_base(
            m.model
                .panes
                .iter()
                .find(|p| p.id == pane)
                .and_then(|p| p.cwd.as_deref()),
        ),
    };
    mention::suggest(&label, &fallback, |n| {
        m.model
            .mentions
            .iter()
            .any(|x| x.workspace == ws && x.name == n)
    })
}

fn kind_of(app: &App, pane: &str) -> &'static str {
    if run_of(app.m(), pane).is_some() {
        "agent"
    } else {
        "terminal"
    }
}

const BAD_NAME: &str = "a name starts with a letter and uses a-z, 0-9, - and _";

// ---- opening and palette actions ---------------------------------------------------------------

/// `prefix+@`: open the picker over the focused pane.
pub fn open(app: &mut App) {
    if app.focused_pane().is_none() {
        app.toast("no focused pane");
        return;
    }
    app.mode = Mode::Popup(Popup::MentionPicker(Box::default()));
}

/// Palette **Name this pane**: ask for the name, prefilled with the current or suggested one.
pub fn open_name_prompt(app: &mut App) {
    let Some(pane) = app.focused_pane() else {
        app.toast("no focused pane");
        return;
    };
    let initial = name_or_suggestion(app, &pane);
    app.mode = Mode::Prompt(crate::app::Prompt {
        kind: PromptKind::MentionName,
        label: "name this pane (@name)".into(),
        input: initial,
    });
}

/// The answer of the **Name this pane** prompt.
pub fn submit_name(app: &mut App, v: &str) {
    let name = v.trim().trim_start_matches('@').to_ascii_lowercase();
    if !mention::valid(&name) {
        app.toast(format!("✗ {BAD_NAME}"));
        return;
    }
    let Some(pane) = app.focused_pane() else {
        return;
    };
    let kind = kind_of(app, &pane);
    app.command(
        "mention.set",
        json!({"pane": pane, "name": name, "kind": kind}),
        Pending::Toast(format!("named @{name}")),
    );
}

/// Palette **Remove this pane's name**.
pub fn clear_name(app: &mut App) {
    let Some(pane) = app.focused_pane() else {
        return;
    };
    if name_of(app.m(), &pane).is_none() {
        app.toast("this pane has no name");
        return;
    }
    app.command(
        "mention.clear",
        json!({"pane": pane}),
        Pending::Toast("name removed".into()),
    );
}

// ---- keys --------------------------------------------------------------------------------------

fn reopen(app: &mut App, p: Box<Picker>) {
    app.mode = Mode::Popup(Popup::MentionPicker(p));
}

pub fn key(app: &mut App, ev: KeyEvent, mut p: Box<Picker>) {
    if ev.kind == KeyKind::Release {
        return reopen(app, p);
    }
    if p.edit.is_some() {
        return edit_key(app, ev, p);
    }
    p.error = None;
    let rows = ranked(app, &p);
    let last = rows.len().saturating_sub(1);
    match ev.key {
        Key::Named(NamedKey::Tab) if !ev.mods.shift() => {
            let i = p.sel.min(last);
            accept(app, p, i, true)
        }
        Key::Named(NamedKey::Tab) => {
            p.sel = p.sel.saturating_sub(1);
            reopen(app, p)
        }
        Key::Char('e') if ev.mods.ctrl() => {
            if let Some(e) = rows.get(p.sel.min(last)) {
                p.edit = Some(Edit {
                    pane: e.pane.clone(),
                    input: e.shown.clone(),
                });
            }
            reopen(app, p)
        }
        _ => match list_key(&ev, p.filter.clone(), p.sel, rows.len()) {
            ListKey::Close => {}
            ListKey::Enter(i) | ListKey::EnterAlt(i) => accept(app, p, i.min(last), false),
            ListKey::Stay(filter, sel) => {
                p.filter = filter;
                p.sel = sel;
                reopen(app, p)
            }
        },
    }
}

/// Keys while a name is being edited: type, backspace, enter confirms, esc cancels.
fn edit_key(app: &mut App, ev: KeyEvent, mut p: Box<Picker>) {
    let Some(mut ed) = p.edit.take() else {
        return reopen(app, p);
    };
    match ev.key {
        Key::Named(NamedKey::Escape) => {
            p.error = None;
            return reopen(app, p);
        }
        Key::Named(NamedKey::Enter) => {
            if !mention::valid(&ed.input) {
                p.error = Some(BAD_NAME.into());
            } else if taken_by_other(app, &ed.pane, &ed.input, &p.overrides) {
                p.error = Some(format!("@{} is already taken", ed.input));
            } else {
                p.error = None;
                p.overrides.retain(|(id, _)| *id != ed.pane);
                p.overrides.push((ed.pane, ed.input));
                return reopen(app, p);
            }
        }
        Key::Named(NamedKey::Backspace) => {
            ed.input.pop();
        }
        Key::Char('u') if ev.mods.ctrl() => ed.input.clear(),
        Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => {
            let c = c.to_ascii_lowercase();
            if (c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
                && ed.input.len() < MAX_LEN
            {
                ed.input.push(c);
            }
        }
        _ => {}
    }
    p.edit = Some(ed);
    reopen(app, p);
}

/// Another pane of the workspace has `name` (as its mention or as an edit in this picker).
/// Names are unique per workspace only: a pane of another workspace may have the same one.
fn taken_by_other(app: &App, pane: &str, name: &str, overrides: &[(String, String)]) -> bool {
    let m = app.m();
    let ws = m
        .model
        .panes
        .iter()
        .find(|p| p.id == pane)
        .map(|p| p.workspace.as_str());
    m.model
        .mentions
        .iter()
        .any(|x| x.name == name && x.pane != pane && Some(x.workspace.as_str()) == ws)
        || overrides.iter().any(|(id, n)| id != pane && n == name)
}

/// Enter (`keep_open` false) or Tab on row `i`.
fn accept(app: &mut App, mut p: Box<Picker>, i: usize, keep_open: bool) {
    let rows = ranked(app, &p);
    let (Some(e), Some(target)) = (rows.get(i), app.focused_pane()) else {
        return reopen(app, p);
    };
    if p.busy {
        return reopen(app, p);
    }
    if !e.needs_set() {
        let (cur, name) = (app.cur, e.shown.clone());
        app.paste_to(cur, &target, format!("@{name} "));
        if keep_open {
            p.inserted.push(name);
            reopen(app, p);
        }
        return;
    }
    if !mention::valid(&e.shown) {
        p.error = Some(BAD_NAME.into());
        return reopen(app, p);
    }
    p.busy = true;
    let params = json!({"pane": e.pane, "name": e.shown, "kind": kind_of(app, &e.pane)});
    let reply = Reply::Set {
        target,
        name: e.shown.clone(),
        keep_open,
    };
    reopen(app, p);
    app.command("mention.set", params, Pending::Mention(reply));
}

/// The server's answer to a picker `mention.set`.
pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    let Reply::Set {
        target,
        name,
        keep_open,
    } = r;
    let open = matches!(app.mode, Mode::Popup(Popup::MentionPicker(_)));
    match res {
        Ok(_) => {
            if !open {
                app.toast(format!("named @{name}"));
                return;
            }
            app.paste_to(mi, &target, format!("@{name} "));
            if let Mode::Popup(Popup::MentionPicker(p)) = &mut app.mode {
                p.busy = false;
                p.error = None;
                p.overrides.retain(|(_, n)| *n != name);
                if keep_open {
                    p.inserted.push(name);
                }
            }
            if !keep_open {
                app.mode = Mode::Normal;
            }
        }
        Err(e) => {
            let msg = if e.reason() == Some("name_taken") {
                format!("@{name} is already taken")
            } else {
                e.message
            };
            if let Mode::Popup(Popup::MentionPicker(p)) = &mut app.mode {
                p.busy = false;
                p.error = Some(msg);
            } else {
                app.toast(format!("✗ {msg}"));
            }
        }
    }
    app.dirty = true;
}

// ---- drawing -----------------------------------------------------------------------------------

/// Where the popup goes: below the focused pane's cursor (above when there is no room),
/// clamped to the pane area; centered when the cursor is hidden or the pane is not visible.
fn place(app: &App, w: u16, h: u16) -> SRect {
    let area = app.pane_area();
    let centered = SRect {
        x: area.x + area.w.saturating_sub(w) / 2,
        y: area.y + area.h.saturating_sub(h) / 3,
        w,
        h,
    };
    let Some(pane) = app.focused_pane() else {
        return centered;
    };
    let Some(buf) = app.m().panes.get(&pane) else {
        return centered;
    };
    let Some((_, r)) = app.pane_rects().into_iter().find(|(id, _)| *id == pane) else {
        return centered;
    };
    if !buf.cursor.visible || buf.cursor.col >= r.w || buf.cursor.row >= r.h {
        return centered;
    }
    let (cx, cy) = (r.x + buf.cursor.col, r.y + buf.cursor.row);
    let (right, bottom) = (area.x + area.w, area.y + area.h);
    let x = cx.min(right.saturating_sub(w)).max(area.x);
    let y = if cy + 1 + h <= bottom {
        cy + 1
    } else if cy >= area.y + h {
        cy - h
    } else {
        bottom.saturating_sub(h).max(area.y)
    };
    SRect { x, y, w, h }
}

/// Draw the picker; returns the text cursor.
pub fn draw(app: &App, g: &mut Grid, p: &Picker) -> (u16, u16) {
    let t = app.theme;
    let rows = ranked(app, p);
    let area = app.pane_area();
    let w = WIDTH.min(area.w.saturating_sub(2)).max(20);
    let h = (rows.len().max(1) as u16 + 4)
        .clamp(6, 12)
        .min(area.h.saturating_sub(1))
        .max(5);
    let r = place(app, w, h);
    let _ = crate::popups::frame_at(app, g, r, "mention a pane");
    let inner_w = r.w.saturating_sub(4);
    // Filter or name editor.
    let (prefix, text) = match &p.edit {
        Some(ed) => ("name @", ed.input.as_str()),
        None => ("> ", p.filter.as_str()),
    };
    g.put_str(r.x + 2, r.y + 1, prefix, t.dim(), inner_w);
    let px = UnicodeWidthStr::width(prefix) as u16;
    g.put_str(
        r.x + 2 + px,
        r.y + 1,
        text,
        t.bold(t.fg),
        inner_w.saturating_sub(px),
    );
    let cursor = (
        (r.x + 2 + px + UnicodeWidthStr::width(text) as u16).min(r.x + r.w.saturating_sub(2)),
        r.y + 1,
    );
    // Rows.
    let cap = h.saturating_sub(4) as usize;
    let skip = p.sel.saturating_sub(cap.saturating_sub(1));
    for (n, (i, e)) in rows.iter().enumerate().skip(skip).take(cap).enumerate() {
        let mut segs = Vec::new();
        if e.name.is_some() {
            segs.push((format!("@{}", e.shown), t.bold(t.accent)));
        } else {
            let edited = p.overrides.iter().any(|(id, _)| *id == e.pane);
            let st = if edited { t.s(t.accent) } else { t.dim() };
            segs.push((format!("→ @{}", e.shown), st));
        }
        segs.push(("  ".into(), t.dim()));
        segs.push((e.label.clone(), t.text()));
        let at = SRect {
            x: r.x + 1,
            y: r.y + 2 + n as u16,
            w: r.w.saturating_sub(2),
            h: 1,
        };
        list_row(g, at, segs, e.state, i == p.sel, app);
    }
    if rows.is_empty() {
        g.put_str(
            r.x + 2,
            r.y + 2,
            "no other panes in this workspace",
            t.dim(),
            inner_w,
        );
    }
    // Footer: an error, the inserted names or the keys.
    let (foot, st) = if let Some(err) = &p.error {
        (format!("✗ {err}"), t.s(t.red))
    } else if p.edit.is_some() {
        ("enter keeps the name · esc cancels".to_string(), t.dim())
    } else if !p.inserted.is_empty() {
        let names: Vec<String> = p.inserted.iter().map(|n| format!("@{n}")).collect();
        (format!("inserted {}", names.join(" ")), t.dim())
    } else {
        (
            "enter inserts · tab keeps open · ctrl+e edits the name · esc".to_string(),
            t.dim(),
        )
    };
    g.put_str(r.x + 2, r.y + h - 2, &foot, st, inner_w);
    cursor
}

#[cfg(test)]
#[path = "mention_tests.rs"]
mod tests;
