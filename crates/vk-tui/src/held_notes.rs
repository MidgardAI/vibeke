//! Held design-review notes (spec 20 §7.1): a reviewer's notes that wait for the owner before
//! they reach the agent.
//!
//! - **Event** `browser.annotations_held {draft, preview, author, count}` (server): the draft is
//!   kept here per machine, a toast says who sent how many notes on which preview, and the
//!   attention inbox gets an item (kind `held_notes`). A later `browser.annotations_sent` or a
//!   `browser.annotations_changed` whose state is not `held` (the owner answered elsewhere, for
//!   example in the app) drops it.
//! - **Overlay** (`Popup::HeldNotes`, opened with enter on the inbox item): the reviewer, the
//!   preview and the notes (`browser.annotations {draft}`), then **Send** (`s` or enter:
//!   `browser.annotations_release {draft}`, which delivers the draft to the agent) or **Discard**
//!   (`d`, then `y`: `browser.annotations_discard {draft}`).
//!
//! Notes are text from another person: control characters are removed and lines are cut to the
//! width of the view.

use serde_json::{Value, json};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::inbox::{Item, ItemKey};
use crate::links::login_of;
use crate::plugins::sanitize;
use crate::screen::Grid;
use vk_proto::render::Style;

/// A draft waiting for the owner.
#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    pub mi: usize,
    pub draft: String,
    /// The preview's id or handle, as the event names it.
    pub preview: String,
    /// "@alice", or the label the reviewer gave.
    pub author: String,
    pub count: u64,
    pub at_ms: i64,
}

/// One note of the open draft.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteLine {
    pub n: u64,
    pub kind: String,
    pub target: String,
    pub text: String,
}

/// The open overlay.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub mi: usize,
    pub draft: String,
    pub preview: String,
    pub author: String,
    pub notes: Vec<NoteLine>,
    pub page_note: Option<String>,
    pub loading: bool,
    pub busy: Option<String>,
    pub error: Option<String>,
    /// `d` was pressed: `y` discards.
    pub confirm: bool,
    /// First body row shown (`j`/`k`, arrows, page keys): the owner reads every note in full
    /// before sending.
    pub scroll: usize,
}

#[derive(Default)]
pub struct State {
    pub items: Vec<Held>,
    pub view: Option<View>,
}

#[derive(Debug, Clone)]
pub enum Reply {
    Draft { draft: String },
    Release { draft: String },
    Discard { draft: String },
}

fn pend(r: Reply) -> Pending {
    Pending::Ux(crate::ux::Reply::Held(r))
}

fn now_ms() -> i64 {
    crate::time::SystemTime::now()
        .duration_since(crate::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A field of an event: from `data`, else from `subject`.
fn field<'a>(v: &'a Value, k: &str) -> &'a Value {
    let d = &v["data"][k];
    if d.is_null() { &v["subject"][k] } else { d }
}

/// Who wrote the notes: a GitHub login as "@login", else the label, else "a reviewer".
pub fn author_text(v: &Value) -> String {
    let label = |x: &Value| {
        x.get("label")
            .and_then(Value::as_str)
            .map(|s| sanitize(s, 40))
            .filter(|s| !s.is_empty())
    };
    let t = match v {
        Value::String(s) => sanitize(s, 40),
        Value::Object(_) if v.get("kind").and_then(Value::as_str) == Some("github") => login_of(v)
            .map(|l| format!("@{l}"))
            .or_else(|| {
                label(v).map(|l| {
                    if l.contains(' ') {
                        l
                    } else {
                        format!("@{}", l.trim_start_matches('@'))
                    }
                })
            })
            .unwrap_or_default(),
        Value::Object(_) => label(v).unwrap_or_default(),
        _ => String::new(),
    };
    if t.is_empty() { "a reviewer".into() } else { t }
}

// ---- events --------------------------------------------------------------------------------------

/// `browser.annotations_held`: remember the draft, say so, and raise the attention item.
pub fn on_held(app: &mut App, mi: usize, v: &Value) {
    let Some(draft) = field(v, "draft").as_str().filter(|d| !d.is_empty()) else {
        return;
    };
    let h = Held {
        mi,
        draft: draft.to_string(),
        preview: sanitize(field(v, "preview").as_str().unwrap_or(""), 40),
        author: author_text(field(v, "author")),
        count: field(v, "count").as_u64().unwrap_or(0),
        at_ms: now_ms(),
    };
    app.toast(format!(
        "✎ {} held note{} from {}{} · open the inbox to send or discard",
        h.count,
        if h.count == 1 { "" } else { "s" },
        h.author,
        if h.preview.is_empty() {
            String::new()
        } else {
            format!(" on {}", h.preview)
        }
    ));
    let list = &mut app.ux.held.items;
    match list.iter_mut().find(|x| x.mi == mi && x.draft == h.draft) {
        Some(x) => *x = h,
        None => list.push(h),
    }
    crate::inbox::invalidate(app);
    app.dirty = true;
}

/// `browser.annotations_sent` / `browser.annotations_changed`: a held draft that is no longer
/// held is dropped (and the overlay for it closed).
pub fn on_settled(app: &mut App, mi: usize, v: &Value) {
    let Some(draft) = field(v, "draft").as_str() else {
        return;
    };
    if field(v, "state").as_str() == Some("held") {
        return;
    }
    forget(app, mi, draft);
}

fn forget(app: &mut App, mi: usize, draft: &str) {
    let before = app.ux.held.items.len();
    app.ux
        .held
        .items
        .retain(|x| !(x.mi == mi && x.draft == draft));
    if app.ux.held.items.len() != before {
        crate::inbox::invalidate(app);
    }
    if app
        .ux
        .held
        .view
        .as_ref()
        .is_some_and(|w| w.mi == mi && w.draft == draft)
    {
        close(app);
    }
    app.dirty = true;
}

// ---- attention inbox -----------------------------------------------------------------------------

/// Held drafts as attention-inbox items (kind `held_notes`), oldest first.
pub fn inbox_items(app: &App) -> Vec<Item> {
    let now = now_ms();
    let mut v: Vec<Item> = app
        .ux
        .held
        .items
        .iter()
        .filter_map(|h| {
            let m = app.machines.get(h.mi)?;
            Some(Item {
                key: ItemKey {
                    machine: h.mi,
                    kind: "held_notes".into(),
                    id: h.draft.clone(),
                },
                class: 3,
                title: format!(
                    "{} held note{} from {}",
                    h.count,
                    if h.count == 1 { "" } else { "s" },
                    h.author
                ),
                subtitle: if h.preview.is_empty() {
                    "design review".into()
                } else {
                    format!("design review of {}", h.preview)
                },
                task: None,
                run: None,
                pane: None,
                interaction: None,
                explanation: "Enter shows the notes: send them to the agent or discard them."
                    .into(),
                age_ms: (now - h.at_ms).max(0),
                risk: None,
                effort: None,
                blocks_tasks: 0,
                effort_estimate: None,
                snoozed_until_ms: None,
                woke_from_snooze: None,
                item_rev: None,
                urgent: false,
                raw_key: json!({"kind": "held_notes", "id": h.draft}),
                fallback: false,
                stale: !m.connected(),
                deadline_ms: None,
                deadline_source: None,
                batch: None,
            })
        })
        .collect();
    v.sort_by_key(|i| std::cmp::Reverse(i.age_ms));
    v
}

// ---- overlay -------------------------------------------------------------------------------------

/// Parse the notes of a `browser.annotations` answer.
pub fn notes_of(draft: &Value) -> Vec<NoteLine> {
    draft["notes"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|n| {
                    let target = ["selector", "name", "text"]
                        .iter()
                        .find_map(|k| n.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))
                        .map(|s| sanitize(s, 60))
                        .unwrap_or_default();
                    NoteLine {
                        n: n["n"].as_u64().unwrap_or(0),
                        kind: sanitize(n["kind"].as_str().unwrap_or(""), 12),
                        target,
                        // In full: the owner approves exactly what the agent gets.
                        text: keep_lines(n["note"].as_str().unwrap_or("")),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A note's text for showing in full: control characters removed line by line, so its line
/// breaks survive.
fn keep_lines(s: &str) -> String {
    s.split('\n')
        .map(|l| sanitize(l, 8192))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Open the overlay for held draft `draft` on machine `mi`.
pub fn open(app: &mut App, mi: usize, draft: &str) {
    if !app.machines.get(mi).is_some_and(|m| m.connected()) {
        app.toast("that machine is offline");
        return;
    }
    let held = app
        .ux
        .held
        .items
        .iter()
        .find(|h| h.mi == mi && h.draft == draft)
        .cloned();
    app.ux.held.view = Some(View {
        mi,
        draft: draft.to_string(),
        preview: held.as_ref().map(|h| h.preview.clone()).unwrap_or_default(),
        author: held
            .as_ref()
            .map_or_else(|| "a reviewer".into(), |h| h.author.clone()),
        notes: Vec::new(),
        page_note: None,
        loading: true,
        busy: None,
        error: None,
        confirm: false,
        scroll: 0,
    });
    app.mode = Mode::Popup(Popup::HeldNotes);
    // By preview, the server lists every draft of the target with its author and state; the
    // held one is picked out by id. Without a preview, ask for the draft itself.
    let preview = app
        .ux
        .held
        .view
        .as_ref()
        .map(|v| v.preview.clone())
        .unwrap_or_default();
    let params = if preview.is_empty() {
        json!({"draft": draft})
    } else {
        json!({"preview": preview})
    };
    app.command_on(
        mi,
        "browser.annotations",
        params,
        pend(Reply::Draft {
            draft: draft.to_string(),
        }),
    );
}

/// The draft `id` in a `browser.annotations` answer: `drafts: [...]` or `draft`.
pub fn pick_draft<'a>(x: &'a Value, id: &str) -> Option<&'a Value> {
    x["drafts"]
        .as_array()
        .and_then(|a| a.iter().find(|d| d["id"].as_str() == Some(id)))
        .or_else(|| x["draft"].is_object().then_some(&x["draft"]))
}

fn close(app: &mut App) {
    app.ux.held.view = None;
    if matches!(app.mode, Mode::Popup(Popup::HeldNotes)) {
        app.restore_return();
    }
}

/// `params` of the call that sends a held draft to the agent.
pub fn release_params(draft: &str) -> Value {
    json!({"draft": draft})
}

/// `params` of the call that discards a held draft.
pub fn discard_params(draft: &str) -> Value {
    json!({"draft": draft})
}

fn send(app: &mut App) {
    let Some(v) = app.ux.held.view.as_mut() else {
        return;
    };
    if v.busy.is_some() {
        return;
    }
    v.busy = Some("sending the notes to the agent…".into());
    v.error = None;
    let (mi, draft) = (v.mi, v.draft.clone());
    app.command_on(
        mi,
        "browser.annotations_release",
        release_params(&draft),
        pend(Reply::Release { draft }),
    );
}

fn discard(app: &mut App) {
    let Some(v) = app.ux.held.view.as_mut() else {
        return;
    };
    if v.busy.is_some() {
        return;
    }
    v.busy = Some("discarding the notes…".into());
    v.error = None;
    v.confirm = false;
    let (mi, draft) = (v.mi, v.draft.clone());
    app.command_on(
        mi,
        "browser.annotations_discard",
        discard_params(&draft),
        pend(Reply::Discard { draft }),
    );
}

pub fn key(app: &mut App, ev: KeyEvent) {
    app.mode = Mode::Popup(Popup::HeldNotes);
    if ev.kind == KeyKind::Release {
        return;
    }
    app.dirty = true;
    let Some(v) = app.ux.held.view.as_mut() else {
        app.restore_return();
        return;
    };
    let plain = !ev.mods.ctrl() && !ev.mods.alt();
    if v.confirm {
        v.confirm = false;
        if plain && matches!(ev.key, Key::Char('y')) {
            discard(app);
        }
        return;
    }
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') if plain => close(app),
        Key::Named(NamedKey::Enter) => send(app),
        Key::Char('s') if plain => send(app),
        Key::Char('d') if plain => v.confirm = true,
        Key::Char('j') | Key::Named(NamedKey::Down) if plain => v.scroll += 1,
        Key::Char('k') | Key::Named(NamedKey::Up) if plain => v.scroll = v.scroll.saturating_sub(1),
        Key::Named(NamedKey::PageDown) | Key::Char(' ') if plain => v.scroll += 10,
        Key::Named(NamedKey::PageUp) if plain => v.scroll = v.scroll.saturating_sub(10),
        _ => {}
    }
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    app.dirty = true;
    match r {
        Reply::Draft { draft } => {
            let Some(v) = app
                .ux
                .held
                .view
                .as_mut()
                .filter(|v| v.mi == mi && v.draft == draft)
            else {
                return;
            };
            v.loading = false;
            match res {
                Ok(x) if pick_draft(&x, &draft).is_some() => {
                    let d = pick_draft(&x, &draft).unwrap();
                    v.notes = notes_of(d);
                    v.page_note = d["page_note"]
                        .as_str()
                        .map(keep_lines)
                        .filter(|s| !s.is_empty());
                    if d["author"].is_object() {
                        v.author = author_text(&d["author"]);
                    }
                }
                Ok(_) => v.error = Some("these notes are gone".into()),
                Err(e) => v.error = Some(e.message),
            }
        }
        Reply::Release { draft } => match res {
            Ok(_) => {
                let n = app
                    .ux
                    .held
                    .items
                    .iter()
                    .find(|h| h.mi == mi && h.draft == draft)
                    .map_or(0, |h| h.count);
                forget(app, mi, &draft);
                app.toast(if n > 0 {
                    format!(
                        "sent {n} note{} to the agent",
                        if n == 1 { "" } else { "s" }
                    )
                } else {
                    "sent the notes to the agent".to_string()
                });
            }
            Err(e) => settle_error(app, mi, &draft, e),
        },
        Reply::Discard { draft } => match res {
            Ok(_) => {
                forget(app, mi, &draft);
                app.toast("discarded the held notes");
            }
            Err(e) => settle_error(app, mi, &draft, e),
        },
    }
}

/// A failed send or discard: shown in the overlay.
fn settle_error(app: &mut App, mi: usize, draft: &str, e: RpcErr) {
    if let Some(v) = app
        .ux
        .held
        .view
        .as_mut()
        .filter(|v| v.mi == mi && v.draft == draft)
    {
        v.busy = None;
        v.error = Some(if e.is_method_not_found() {
            "this machine's vibeke cannot release held notes (update it)".to_string()
        } else {
            e.message
        });
    }
}

pub fn draw(app: &App, g: &mut Grid) {
    let t = app.theme;
    let mut a = crate::drafts::Area::open(app, g, "Vibeke · Held review notes");
    let Some(v) = app.ux.held.view.as_ref() else {
        return;
    };
    a.line(
        &format!(
            "{} left notes{}",
            v.author,
            if v.preview.is_empty() {
                String::new()
            } else {
                format!(" on {}", v.preview)
            }
        ),
        t.bold(t.fg),
    );
    a.line(
        "They wait for you. Send them to the agent, or discard them.",
        t.dim(),
    );
    a.line("", t.text());
    if v.loading {
        a.line("  loading…", t.dim());
    }
    // Every note in full, wrapped; the body scrolls when it is taller than the view.
    let width = (a.r.w as usize).saturating_sub(5).max(10);
    let mut body: Vec<(String, Style)> = Vec::new();
    for n in &v.notes {
        let target = if n.target.is_empty() {
            String::new()
        } else {
            format!(" {}", n.target)
        };
        body.push((format!("{}. [{}]{target}", n.n, n.kind), t.bold(t.fg)));
        for para in n.text.lines() {
            for row in crate::assist::wrap(para, width) {
                body.push((format!("   {row}"), t.text()));
            }
        }
    }
    if let Some(p) = &v.page_note {
        body.push((String::new(), t.text()));
        body.push(("Page note:".into(), t.bold(t.fg)));
        for para in p.lines() {
            for row in crate::assist::wrap(para, width) {
                body.push((format!("   {row}"), t.text()));
            }
        }
    }
    // Room for the status line and its blank row.
    let room = (a.left() as usize).saturating_sub(2).max(1);
    let max = body.len().saturating_sub(room);
    let start = v.scroll.min(max);
    for (line, st) in body.iter().skip(start).take(room) {
        a.line(line, *st);
    }
    let more = body.len() > room;
    a.line("", t.text());
    if v.confirm {
        a.line(
            "Discard these notes? The agent never sees them. [y] discard  [other] keep",
            t.bold(t.red),
        );
    } else if let Some(b) = &v.busy {
        a.line(&format!("⏳ {b}"), t.s(t.yellow));
    } else if let Some(e) = &v.error {
        a.line(&format!("✗ {e}"), t.s(t.red));
    }
    a.footer(
        if more {
            "s/enter send to the agent · d discard · j/k scroll · esc close"
        } else {
            "s/enter send to the agent · d discard · esc close"
        },
        t.dim(),
    );
}

#[cfg(test)]
#[path = "held_notes_tests.rs"]
mod tests;
