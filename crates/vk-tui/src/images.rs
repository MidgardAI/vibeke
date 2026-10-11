//! Recent images, the unseen state and the sidebar **Images** section (spec 18 §4).
//!
//! - **Recent cache**: per machine, the newest `ui.images.recent` records from `screenshot.list
//!   {limit}` (metadata only) and the server's `total`. Asked for on connect and reconnect, and
//!   again after `screenshot.captured` and `screenshot.deleted` (coalesced: one request per
//!   machine at the next tick). A reply older than the last one applied for that machine is
//!   dropped (sequence numbers per machine).
//! - **Unseen set**: `(machine, screenshot id)` of images captured after this client connected
//!   (from `screenshot.captured`; the subscription replays what a reconnect missed). Images that
//!   existed before count as seen. An image becomes seen when the viewer shows it, or when the
//!   gallery's full view shows it. The 📷N badges count unseen images per `(machine, pane)`.
//! - **Deletions** (`screenshot.deleted` `data.ids`, also by another client or by retention)
//!   leave the unseen set, the recent cache, cached pixels, the viewer and any pending removal.
//! - **Sidebar rows**: right after the needs-you section, newest first with unseen ones first,
//!   at most `ui.images.recent` rows and a third of the sidebar height, then `+N more on <machine>`
//!   per machine. `enter`/click opens the viewer; `x` removes with the viewer's confirm rules.

use crate::app::{App, Mode, Pending, RpcErr};
use crate::drafts::arr;
use crate::draw::SideRow;
use crate::gallery::{Reply, Scope, Shot, View};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

/// One machine's newest images and how many the server holds.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Recent {
    /// Newest first (the server's order).
    pub shots: Vec<Shot>,
    pub total: u64,
}

/// An image captured after this client connected that nobody looked at here yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Unseen {
    pub machine: usize,
    pub id: String,
    /// The pane that attached or captured it (badges).
    pub pane: Option<String>,
}

/// What a removal waits for (§5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// An attached image: a second `x`.
    Again,
    /// A captured screenshot: `y` (the gallery's confirm step).
    Yes,
    /// Kept for a review acceptance: `!`.
    Force,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub machine: usize,
    pub id: String,
    pub step: Step,
}

#[derive(Debug, Default)]
pub struct State {
    pub recent: HashMap<usize, Recent>,
    /// Oldest first.
    pub unseen: Vec<Unseen>,
    /// A removal waiting for its second key (viewer or sidebar row).
    pub confirm: Option<Confirm>,
    /// The open image viewer.
    pub viewer: Option<crate::viewer::Viewer>,
    /// The attach pop-up card (spec 18 §6).
    pub card: Option<crate::card::Card>,
    /// Machines whose recent list is asked for at the next tick.
    stale: BTreeSet<usize>,
    /// Last request sequence number sent / applied, per machine.
    sent: HashMap<usize, u64>,
    applied: HashMap<usize, u64>,
}

// ---- recent cache --------------------------------------------------------------------------------

/// After (re)connecting machine `mi`: ask for its recent images.
pub fn on_connected(app: &mut App, mi: usize) {
    app.images_ui.stale.insert(mi);
}

/// Ask for the recent list of every machine marked stale (once per tick, however many events
/// arrived).
pub fn tick(app: &mut App) {
    let stale = std::mem::take(&mut app.images_ui.stale);
    for mi in stale {
        if app.machines.get(mi).is_some_and(|m| m.connected()) {
            refresh(app, mi);
        }
    }
}

fn refresh(app: &mut App, mi: usize) {
    let seq = {
        let s = app.images_ui.sent.entry(mi).or_insert(0);
        *s += 1;
        *s
    };
    let limit = app.config.ui.images.recent.clamp(1, 20);
    app.command_on(
        mi,
        "screenshot.list",
        json!({"limit": limit}),
        Pending::Gallery(Reply::Recent { seq }),
    );
}

/// The reply to [`refresh`]: replaces machine `mi`'s cache unless a newer reply was applied.
pub fn on_recent(app: &mut App, mi: usize, seq: u64, res: Result<Value, RpcErr>) {
    if app.images_ui.applied.get(&mi).is_some_and(|a| *a >= seq) {
        return;
    }
    // An older server without screenshot records, or a failed call: keep what there is.
    let Ok(x) = res else { return };
    app.images_ui.applied.insert(mi, seq);
    let shots: Vec<Shot> = arr(&x, "screenshots")
        .iter()
        .filter_map(Shot::parse)
        .collect();
    let total = x
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .max(shots.len() as u64);
    app.images_ui.recent.insert(mi, Recent { shots, total });
    app.dirty = true;
    crate::card::on_recent(app, mi);
}

/// A pushed `screenshot.captured`: the image is unseen; the recent list is asked for again.
pub fn on_captured(app: &mut App, mi: usize, v: &Value) {
    let id = v["data"]["id"]
        .as_str()
        .or_else(|| v["subject"]["screenshot"].as_str())
        .filter(|s| !s.is_empty());
    if let Some(id) = id
        && !is_unseen(app, mi, id)
    {
        let pane = v["subject"]["pane"]
            .as_str()
            .or_else(|| v["data"]["pane"].as_str())
            .map(str::to_string);
        app.images_ui.unseen.push(Unseen {
            machine: mi,
            id: id.to_string(),
            pane,
        });
    }
    app.images_ui.stale.insert(mi);
}

/// A pushed `screenshot.deleted` (`data.ids`): by this client, another one or retention.
pub fn on_deleted(app: &mut App, mi: usize, ids: &[String]) {
    forget(app, mi, ids);
    app.images_ui.stale.insert(mi);
}

/// Screenshots `ids` of machine `mi` are gone: drop them from every image surface.
pub fn forget(app: &mut App, mi: usize, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let gone = |m: usize, id: &str| m == mi && ids.iter().any(|x| x == id);
    let st = &mut app.images_ui;
    st.unseen.retain(|u| !gone(u.machine, &u.id));
    if let Some(r) = st.recent.get_mut(&mi) {
        let before = r.shots.len();
        r.shots.retain(|s| !ids.contains(&s.id));
        r.total = r.total.saturating_sub((before - r.shots.len()) as u64);
    }
    if st.confirm.as_ref().is_some_and(|c| gone(c.machine, &c.id)) {
        st.confirm = None;
    }
    for id in ids {
        app.imgview.forget(mi, id);
    }
    crate::viewer::on_forgotten(app, mi, ids);
    crate::card::on_forgotten(app, mi, ids);
    // A navigate-mode selection past the last row moves back onto it.
    if let Mode::Navigate { sel } = app.mode {
        let n = crate::draw::sidebar_targets(app).len();
        if n > 0 && sel >= n {
            app.mode = Mode::Navigate { sel: n - 1 };
        }
    }
    app.dirty = true;
}

// ---- unseen state --------------------------------------------------------------------------------

pub fn is_unseen(app: &App, mi: usize, id: &str) -> bool {
    app.images_ui
        .unseen
        .iter()
        .any(|u| u.machine == mi && u.id == id)
}

/// The image was looked at (viewer, gallery full view): its badge and bright row go.
pub fn mark_seen(app: &mut App, mi: usize, id: &str) {
    let before = app.images_ui.unseen.len();
    app.images_ui
        .unseen
        .retain(|u| !(u.machine == mi && u.id == id));
    if app.images_ui.unseen.len() != before {
        app.dirty = true;
    }
}

/// Unseen images, newest first.
pub fn unseen(app: &App) -> Vec<(usize, String)> {
    app.images_ui
        .unseen
        .iter()
        .rev()
        .map(|u| (u.machine, u.id.clone()))
        .collect()
}

/// Unseen images attached or captured by `pane` on machine `mi` (the 📷N badge).
pub fn unseen_count(app: &App, mi: usize, pane: &str) -> u32 {
    app.images_ui
        .unseen
        .iter()
        .filter(|u| u.machine == mi && u.pane.as_deref() == Some(pane))
        .count() as u32
}

/// The newest unseen image of `pane` that the recent cache knows.
pub fn newest_unseen_of(app: &App, mi: usize, pane: &str) -> Option<(usize, String)> {
    app.images_ui
        .unseen
        .iter()
        .rev()
        .find(|u| {
            u.machine == mi && u.pane.as_deref() == Some(pane) && shot(app, mi, &u.id).is_some()
        })
        .map(|u| (u.machine, u.id.clone()))
}

// ---- lookups -------------------------------------------------------------------------------------

/// Screenshot `id` of machine `mi` from the recent cache.
pub fn shot<'a>(app: &'a App, mi: usize, id: &str) -> Option<&'a Shot> {
    app.images_ui
        .recent
        .get(&mi)?
        .shots
        .iter()
        .find(|s| s.id == id)
}

/// Every cached image, newest first across machines; unseen ones first when asked.
pub fn ordered(app: &App, unseen_first: bool) -> Vec<(usize, &Shot)> {
    let mut v: Vec<(usize, &Shot)> = app
        .images_ui
        .recent
        .iter()
        .flat_map(|(mi, r)| r.shots.iter().map(move |s| (*mi, s)))
        .collect();
    v.sort_by(|a, b| {
        b.1.created_at_ms
            .cmp(&a.1.created_at_ms)
            .then(a.0.cmp(&b.0))
    });
    if unseen_first {
        v.sort_by_key(|(mi, s)| !is_unseen(app, *mi, &s.id));
    }
    v
}

/// The run that attached or captured an image in `pane`: its name, else its harness. `None`
/// when the pane or run is gone.
pub fn agent(app: &App, mi: usize, pane: Option<&str>) -> Option<String> {
    let pane = pane?;
    let runs = &app.machines.get(mi)?.model.runs;
    runs.iter()
        .find(|r| r.pane == pane && r.ended_at_ms.is_none())
        .or_else(|| runs.iter().find(|r| r.pane == pane))
        .map(|r| r.label().to_string())
}

/// What a row and the viewer call an image: the caption, else the page title or URL, else the
/// file name, else the environment label.
pub fn title(s: &Shot) -> String {
    s.caption
        .clone()
        .or_else(|| (!s.title.is_empty()).then(|| s.title.clone()))
        .or_else(|| (!s.url.is_empty()).then(|| s.url.clone()))
        .or_else(|| s.source_name.clone())
        .unwrap_or_else(|| s.label_text().to_string())
}

// ---- removal -------------------------------------------------------------------------------------

/// `x` on image `(mi, id)`: ask for the second key (a second `x` for an attached image, `y` for
/// a captured screenshot).
pub fn ask_remove(app: &mut App, mi: usize, id: String) {
    let attached = shot(app, mi, &id).is_none_or(Shot::is_agent);
    app.images_ui.confirm = Some(Confirm {
        machine: mi,
        id,
        step: if attached { Step::Again } else { Step::Yes },
    });
}

/// A key while a removal waits. `None`: nothing waits. `Some(true)`: the key was used (removing,
/// or kept after the gallery-style confirm step). `Some(false)`: the removal is off and the key
/// acts as usual (an attached image's second `x` is not a dialog).
pub fn confirm_key(app: &mut App, ev: &KeyEvent) -> Option<bool> {
    let c = app.images_ui.confirm.take()?;
    let force = match (c.step, &ev.key) {
        (Step::Again, Key::Char('x')) | (Step::Yes, Key::Char('y' | 'Y')) => false,
        (Step::Force, Key::Char('!')) => true,
        (Step::Again, _) => return Some(false),
        _ => return Some(true),
    };
    let mut p = json!({"id": c.id});
    if force {
        p["force"] = json!(true);
    }
    app.command_on(
        c.machine,
        "screenshot.delete",
        p,
        Pending::Gallery(Reply::Removed { id: c.id }),
    );
    Some(true)
}

/// The reply to a removal from the viewer or the sidebar.
pub fn on_removed(app: &mut App, mi: usize, id: String, res: Result<Value, RpcErr>) {
    match res {
        // The `screenshot.deleted` event follows; forgetting twice is harmless.
        Ok(_) => forget(app, mi, &[id]),
        Err(e) if e.reason() == Some("referenced_by_acceptance") => {
            app.images_ui.confirm = Some(Confirm {
                machine: mi,
                id,
                step: Step::Force,
            });
        }
        Err(e) => app.toast(format!("✗ {}", e.message)),
    }
    app.dirty = true;
}

/// The prompt a waiting removal shows (viewer action line, sidebar row).
pub fn confirm_text(step: Step) -> &'static str {
    match step {
        Step::Again => "x again to remove",
        Step::Yes => "Remove this screenshot? [y] remove  [any key] keep",
        Step::Force => "Kept for a review acceptance — [!] remove anyway  [any key] keep",
    }
}

/// The step a removal of `(mi, id)` waits for, if one does.
pub fn confirm_for(app: &App, mi: usize, id: &str) -> Option<Step> {
    app.images_ui
        .confirm
        .as_ref()
        .filter(|c| c.machine == mi && c.id == id)
        .map(|c| c.step)
}

// ---- sidebar -------------------------------------------------------------------------------------

/// The Images section (built before the navigate-mode `/` filter, so it can match).
pub fn sidebar_rows(app: &App, rows: &mut Vec<SideRow>) {
    if !app.config.ui.images.sidebar {
        return;
    }
    let all = ordered(app, true);
    if all.is_empty() {
        return;
    }
    let t = &app.theme;
    // At most a third of the sidebar height, the header included.
    let third = (app.size.1.saturating_sub(1) / 3) as usize;
    let max = (app.config.ui.images.recent.clamp(1, 20) as usize)
        .min(third.saturating_sub(1))
        .max(1);
    let new = app.images_ui.unseen.len();
    let head = if new > 0 {
        (format!("─ images · {new} new ─"), t.bold(t.accent))
    } else {
        ("─ images ─".to_string(), t.dim())
    };
    rows.push(SideRow {
        segs: vec![head],
        ..Default::default()
    });
    let mut shown: HashMap<usize, u64> = HashMap::new();
    for (mi, s) in all.iter().take(max) {
        *shown.entry(*mi).or_insert(0) += 1;
        rows.push(image_row(app, *mi, s));
    }
    let multi = app.machines.len() > 1;
    for (mi, m) in app.machines.iter().enumerate() {
        let Some(r) = app.images_ui.recent.get(&mi) else {
            continue;
        };
        let more = r.total.saturating_sub(shown.get(&mi).copied().unwrap_or(0));
        if more > 0 {
            let on = if multi {
                format!(" on {}", m.label)
            } else {
                String::new()
            };
            rows.push(SideRow {
                segs: vec![(format!("  +{more} more{on}"), t.dim())],
                image_more: Some(mi),
                ..Default::default()
            });
        }
    }
    rows.push(SideRow::default());
}

/// `📷 s5 login page · claude · 2m` (plus the machine label for a remote machine).
pub fn row_text(app: &App, mi: usize, s: &Shot) -> String {
    let mut parts = vec![format!("{} {}", s.handle, title(s))];
    if let Some(a) = agent(app, mi, s.pane.as_deref()) {
        parts.push(a);
    }
    if let Some(m) = app.machines.get(mi).filter(|m| !m.local) {
        parts.push(m.label.clone());
    }
    parts.push(crate::gallery::age(s.created_at_ms));
    format!(" 📷 {}", parts.join(" · "))
}

fn image_row(app: &App, mi: usize, s: &Shot) -> SideRow {
    let t = &app.theme;
    let style = if is_unseen(app, mi, &s.id) {
        t.bold(t.accent)
    } else {
        t.dim()
    };
    let mut segs = vec![(row_text(app, mi, s), style)];
    if let Some(step) = confirm_for(app, mi, &s.id) {
        let txt = match step {
            Step::Again => " · x again to remove",
            Step::Yes => " · remove? y",
            Step::Force => " · in a review: ! to remove",
        };
        segs.push((txt.to_string(), t.bold(t.yellow)));
    }
    SideRow {
        segs,
        image: Some((mi, s.id.clone())),
        ..Default::default()
    }
}

/// Navigate-mode keys on an image row (and a `+N more` row): `enter` opens the viewer (the
/// gallery for `+N more`), `x` removes. Checked before every pane action, so an image row never
/// acts on a pane (a digit that jumps to one opens it); moving, `/` and leaving still work.
pub fn navigate_key(app: &mut App, ev: &KeyEvent, sel: usize) -> bool {
    if ev.kind == KeyKind::Release || app.ux.nav.typing {
        return false;
    }
    let stay = |app: &mut App| app.mode = Mode::Navigate { sel };
    if confirm_key(app, ev) == Some(true) {
        stay(app);
        return true;
    }
    let rows = crate::draw::sidebar_rows(app);
    let target = |i: usize| {
        rows.iter()
            .filter(|r| r.selectable())
            .nth(i)
            .map(|r| (r.image.clone(), r.image_more))
            .filter(|(image, more)| image.is_some() || more.is_some())
    };
    let open =
        |app: &mut App, image: Option<(usize, String)>, more: Option<usize>| match (image, more) {
            (Some(t), _) => crate::viewer::open(app, t, false),
            (None, Some(mi)) => crate::gallery::open(app, mi, Scope::All, View::List, false),
            (None, None) => {}
        };
    if let Key::Char(c) = ev.key
        && let Some(d) = c.to_digit(10)
    {
        let Some((image, more)) = target((d as usize).saturating_sub(1)) else {
            return false;
        };
        open(app, image, more);
        return true;
    }
    let Some((image, more)) = target(sel) else {
        return false;
    };
    match ev.key {
        Key::Named(NamedKey::Escape | NamedKey::Down | NamedKey::Up)
        | Key::Char('j' | 'k' | 'q' | '/') => return false,
        Key::Named(NamedKey::Enter) => open(app, image, more),
        Key::Char('x') => {
            if let Some((mi, id)) = image {
                ask_remove(app, mi, id);
            }
            stay(app);
        }
        _ => stay(app),
    }
    true
}

/// Sidebar clicks on image rows, `+N more` rows and an agent row's 📷N badge. Runs before the
/// pane click handlers.
pub fn on_mouse(app: &mut App, me: &crate::event::MouseEvent) -> bool {
    use crate::event::{MouseButton, MouseEventKind};
    if !matches!(me.kind, MouseEventKind::Down(MouseButton::Left))
        || !crate::chrome::in_sidebar(app, me.column)
        || !matches!(app.mode, Mode::Normal | Mode::Navigate { .. })
    {
        return false;
    }
    let rows = crate::draw::sidebar_rows(app);
    let Some(r) = (me.row as usize)
        .checked_sub(1)
        .and_then(|i| rows.into_iter().nth(i))
    else {
        return false;
    };
    if let Some(t) = r.image {
        crate::viewer::open(app, t, false);
        return true;
    }
    if let Some(mi) = r.image_more {
        crate::gallery::open(app, mi, Scope::All, View::List, false);
        return true;
    }
    let Some((mi, pane)) = r.target else {
        return false;
    };
    let rel = me
        .column
        .saturating_sub(crate::chrome::sidebar_x(app).unwrap_or(0));
    let mut x = 0u16;
    let mut on_badge = false;
    for (s, _) in &r.segs {
        let w = unicode_width::UnicodeWidthStr::width(s.as_str()) as u16;
        if s.starts_with(" 📷") && rel >= x && rel < x + w {
            on_badge = true;
        }
        x += w;
    }
    if !on_badge || unseen_count(app, mi, &pane) == 0 {
        return false;
    }
    match newest_unseen_of(app, mi, &pane) {
        Some(t) => crate::viewer::open(app, t, true),
        None => crate::gallery::open(app, mi, Scope::Pane(pane), View::List, false),
    }
    true
}

#[cfg(test)]
#[path = "images_tests.rs"]
pub(crate) mod tests;
