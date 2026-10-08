//! Phone-gateway support in the TUI (spec 16 §13 X3, X5, X6): host-wide presence pings,
//! the out-of-band confirm overlay (chrome only, never in a PTY), `answered_by` display and the
//! connected-devices indicator.

use crate::app::{App, Pending, RpcErr};
use crate::screen::Grid;
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::model::{Interaction, InteractionStatus};

/// Presence ping interval while the user is active (X3).
pub const ACTIVITY_EVERY: Duration = Duration::from_secs(10);
const EVENTS_EVERY: Duration = Duration::from_secs(1);
const LIST_EVERY: Duration = Duration::from_secs(15);
/// With event push the list is refreshed on `client.*` events; this is only a safety net.
const LIST_EVERY_PUSH: Duration = Duration::from_secs(120);
/// A request with no answer for this long is considered lost (disconnect) and re-sent.
const INFLIGHT_MAX: Duration = Duration::from_secs(10);
/// Keys right after an overlay appears are swallowed: they were typed for the pane.
const ARM_DELAY: Duration = Duration::from_millis(600);
/// How long after an answer an `answered on …` toast is still worth showing.
const ANNOUNCE_WINDOW_MS: i64 = 60_000;

#[derive(Debug, Clone)]
pub enum Reply {
    Activity,
    Events,
    List,
    Answer,
}

/// At most one ping per 10 s while active; the first input after idle always pings.
#[derive(Debug, Default)]
pub struct Throttle {
    last: Option<Instant>,
}

impl Throttle {
    pub fn note_input(&mut self, now: Instant) -> bool {
        match self.last {
            Some(t) if now.saturating_duration_since(t) < ACTIVITY_EVERY => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Confirm {
    pub machine: usize,
    pub id: String,
    pub title: String,
    pub body: String,
    pub options: Vec<(String, String)>,
    pub deadline: Instant,
    pub sel: usize,
    pub shown_at: Instant,
}

impl Confirm {
    pub fn remaining(&self, now: Instant) -> Duration {
        self.deadline.saturating_duration_since(now)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Swallowed, nothing to do.
    None,
    Answer {
        machine: usize,
        id: String,
        choice: String,
    },
    /// Esc: hide it without answering (the requester times out on its own).
    Dismissed,
}

#[derive(Default)]
struct Per {
    cursor: i64,
    events_sent: Option<Instant>,
    next_events: Option<Instant>,
    list_sent: Option<Instant>,
    next_list: Option<Instant>,
    devices: u32,
    /// The server pushes our events (`crate::push`): no `events.read` polling once caught up.
    push: bool,
    /// The one catch-up `events.read` after (re)connecting or a lag has completed.
    caught_up: bool,
}

fn ready(sent: Option<Instant>, next: Option<Instant>, now: Instant) -> bool {
    sent.is_none_or(|s| now.saturating_duration_since(s) > INFLIGHT_MAX)
        && next.is_none_or(|n| now >= n)
}

#[derive(Default)]
pub struct State {
    pub throttle: Throttle,
    per: Vec<Per>,
    pub queue: VecDeque<Confirm>,
    dismissed: HashSet<(usize, String)>,
    announced: HashSet<String>,
}

impl State {
    fn per(&mut self, i: usize) -> &mut Per {
        if self.per.len() <= i {
            self.per.resize_with(i + 1, Per::default);
        }
        &mut self.per[i]
    }

    pub fn devices(&self, machine: usize) -> u32 {
        self.per.get(machine).map(|p| p.devices).unwrap_or(0)
    }

    pub fn modal(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Queue a request; duplicates and dismissed ones are ignored.
    pub fn push(&mut self, c: Confirm) {
        let key = (c.machine, c.id.clone());
        if self.dismissed.contains(&key)
            || self
                .queue
                .iter()
                .any(|q| q.machine == c.machine && q.id == c.id)
        {
            return;
        }
        self.queue.push_back(c);
    }

    /// Answered elsewhere or timed out on the server.
    pub fn resolve(&mut self, machine: usize, id: &str) {
        self.retain(|q| !(q.machine == machine && q.id == id), Instant::now());
    }

    /// Keep the cards `keep` accepts; a card that became the front one is armed afresh.
    pub fn retain(&mut self, keep: impl FnMut(&Confirm) -> bool, now: Instant) {
        let front = self.queue.front().map(|c| (c.machine, c.id.clone()));
        self.queue.retain(keep);
        if self.queue.front().map(|c| (c.machine, c.id.clone())) != front {
            self.rearm_front(now);
        }
    }

    /// The card now in front was under another one: keys count only `ARM_DELAY` from now, so
    /// a key meant for the card that went away never answers this one.
    fn rearm_front(&mut self, now: Instant) {
        if let Some(c) = self.queue.front_mut() {
            c.shown_at = c.shown_at.max(now);
        }
    }

    /// Drop requests whose countdown ran out; true when something changed.
    pub fn expire(&mut self, now: Instant) -> bool {
        let n = self.queue.len();
        self.retain(|q| q.deadline > now, now);
        self.queue.len() != n
    }

    pub fn handle_key(&mut self, ev: &KeyEvent, now: Instant) -> KeyOutcome {
        let Some(c) = self.queue.front_mut() else {
            return KeyOutcome::None;
        };
        // Only a fresh press with no modifier at all, once the front card was armed.
        if ev.kind != KeyKind::Press
            || !ev.mods.is_empty()
            || now.saturating_duration_since(c.shown_at) < ARM_DELAY
        {
            return KeyOutcome::None;
        }
        let n = c.options.len();
        let pick = match &ev.key {
            Key::Named(NamedKey::Escape) => {
                let c = self.queue.pop_front().unwrap();
                self.dismissed.insert((c.machine, c.id));
                self.rearm_front(now);
                return KeyOutcome::Dismissed;
            }
            Key::Named(NamedKey::Left | NamedKey::Up) => {
                c.sel = (c.sel + n - 1) % n.max(1);
                None
            }
            Key::Named(NamedKey::Right | NamedKey::Down | NamedKey::Tab) => {
                c.sel = (c.sel + 1) % n.max(1);
                None
            }
            Key::Named(NamedKey::Enter) => Some(c.sel),
            Key::Char(ch) if ch.is_ascii_digit() && *ch != '0' => {
                let i = *ch as usize - '1' as usize;
                (i < n).then_some(i)
            }
            Key::Char(ch) => c.options.iter().position(|(_, l)| {
                l.chars()
                    .next()
                    .is_some_and(|f| f.to_lowercase().eq(ch.to_lowercase()))
            }),
            _ => None,
        };
        match pick.filter(|i| *i < n) {
            Some(i) => {
                let c = self.queue.pop_front().unwrap();
                let choice = c.options[i].0.clone();
                self.dismissed.insert((c.machine, c.id.clone()));
                self.rearm_front(now);
                KeyOutcome::Answer {
                    machine: c.machine,
                    id: c.id,
                    choice,
                }
            }
            None => KeyOutcome::None,
        }
    }
}

// ---- answered_by (X6) ---------------------------------------------------------------------

/// `gateway:<device>` → "answered on <device>"; anything else was not a phone.
pub fn answered_text(by: &str) -> Option<String> {
    let dev = by.strip_prefix("gateway:")?.trim();
    Some(if dev.is_empty() {
        format!("answered on {by}")
    } else {
        format!("answered on {dev}")
    })
}

pub fn interaction_answered_text(it: &Interaction) -> Option<String> {
    if it.status == InteractionStatus::Open {
        return None;
    }
    answered_text(it.answered_by.as_deref()?)
}

/// Inbox row suffix for the interaction behind an item.
pub fn row_suffix(app: &App, machine: usize, interaction: Option<&str>) -> String {
    let Some(id) = interaction else {
        return String::new();
    };
    app.machines
        .get(machine)
        .and_then(|m| m.model.interactions.iter().find(|x| x.id == id))
        .and_then(interaction_answered_text)
        .map(|t| format!(" · {t}"))
        .unwrap_or_default()
}

/// Toast once for interactions a phone just answered.
pub fn on_model(app: &mut App, i: usize) {
    let now = now_ms();
    let mut toasts = Vec::new();
    for it in &app.machines[i].model.interactions {
        if let Some(t) = interaction_answered_text(it)
            && it
                .answered_at_ms
                .is_some_and(|a| now - a <= ANNOUNCE_WINDOW_MS)
            && app.gateway.announced.insert(format!("{i}:{}", it.id))
        {
            toasts.push(format!("{t}: {}", it.title));
        }
    }
    for t in toasts {
        app.toast(t);
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- presence (X3) ------------------------------------------------------------------------

/// Called for every key/mouse/paste event from the host terminal.
pub fn on_input(app: &mut App) {
    if !app.gateway.throttle.note_input(Instant::now()) {
        return;
    }
    let at = now_ms();
    for i in 0..app.machines.len() {
        if app.machines[i].connected() {
            app.command_on(
                i,
                "client.activity",
                json!({"last_input_ms": at}),
                Pending::Gateway(Reply::Activity),
            );
        }
    }
}

// ---- polling ------------------------------------------------------------------------------

pub fn on_connected(app: &mut App, i: usize) {
    app.gateway.retain(|q| q.machine != i, Instant::now());
    *app.gateway.per(i) = Per::default();
}

/// Event push on/off for machine `i` (set after connecting, see `crate::push`).
pub fn set_push(app: &mut App, i: usize, on: bool) {
    let p = app.gateway.per(i);
    p.push = on;
    p.caught_up = false;
}

/// Whether machine `i` is still polled for confirm events.
pub fn polling(app: &mut App, i: usize) -> bool {
    let p = app.gateway.per(i);
    !(p.push && p.caught_up)
}

/// A pushed `client.*` event: refresh the client/device list now.
pub fn refresh_list(app: &mut App, i: usize) {
    if !app.machines[i].connected() {
        return;
    }
    let now = Instant::now();
    let p = app.gateway.per(i);
    if p.list_sent
        .is_some_and(|s| now.saturating_duration_since(s) <= INFLIGHT_MAX)
    {
        // One in flight; ask again right after it answers.
        p.next_list = Some(now);
        return;
    }
    p.list_sent = Some(now);
    p.next_list = Some(now + LIST_EVERY_PUSH);
    app.command_on(i, "client.list", json!({}), Pending::Gateway(Reply::List));
}

/// The server dropped pushed events for us: one more `events.read` from the cursor.
pub fn catch_up(app: &mut App, i: usize) {
    let p = app.gateway.per(i);
    p.caught_up = false;
    p.next_events = None;
}

/// When `tick` next has something to do: the confirm overlay's countdown and expiry, and per
/// connected machine the `events.read` poll (not once the server pushes and we caught up) and
/// the `client.list` refresh at their own intervals, or a re-send after a lost request. With
/// event push the list follows pushed `client.*` events; its 120 s safety net then rides on
/// other wakeups instead of arming a timer of its own (spec 10 §1.3.1).
pub(crate) fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    let g = &app.gateway;
    if let Some(t) = g.queue.iter().map(|q| q.deadline).min() {
        d.at("confirm.expire", t);
    }
    if let Some(c) = g.queue.front() {
        // "expires in Ns" shows whole seconds: repaint when the next one starts.
        let left = c.remaining(now);
        let frac = left - Duration::from_secs(left.as_secs());
        d.redraw(
            "confirm.countdown",
            now + frac.max(Duration::from_millis(1)),
        );
    }
    let due = |sent: Option<Instant>, next: Option<Instant>| {
        let retry = sent.map_or(now, |s| s + INFLIGHT_MAX);
        retry.max(next.unwrap_or(now))
    };
    for (i, m) in app.machines.iter().enumerate() {
        if !m.connected() {
            continue;
        }
        let Some(p) = g.per.get(i) else {
            // Never ticked since connecting: both polls are due.
            d.at("gateway.events", now);
            d.at("gateway.list", now);
            continue;
        };
        if !(p.push && p.caught_up) {
            d.at("gateway.events", due(p.events_sent, p.next_events));
        }
        if !p.push || p.list_sent.is_some() {
            d.at("gateway.list", due(p.list_sent, p.next_list));
        }
    }
}

pub fn tick(app: &mut App) {
    let now = Instant::now();
    if app.gateway.expire(now) || app.gateway.modal() {
        app.dirty = true;
    }
    for i in 0..app.machines.len() {
        if !app.machines[i].connected() {
            continue;
        }
        let (do_events, do_list, cursor, push) = {
            let p = app.gateway.per(i);
            (
                ready(p.events_sent, p.next_events, now) && !(p.push && p.caught_up),
                ready(p.list_sent, p.next_list, now),
                p.cursor,
                p.push,
            )
        };
        if do_events {
            let p = app.gateway.per(i);
            p.events_sent = Some(now);
            p.next_events = Some(now + EVENTS_EVERY);
            app.command_on(
                i,
                "events.read",
                json!({"after": cursor, "types": ["client.confirm_*"], "limit": 100}),
                Pending::Gateway(Reply::Events),
            );
        }
        if do_list {
            let p = app.gateway.per(i);
            p.list_sent = Some(now);
            p.next_list = Some(now + if push { LIST_EVERY_PUSH } else { LIST_EVERY });
            app.command_on(i, "client.list", json!({}), Pending::Gateway(Reply::List));
        }
    }
}

pub fn on_reply(app: &mut App, i: usize, reply: Reply, res: Result<Value, RpcErr>) {
    let now = Instant::now();
    match reply {
        Reply::Activity => {}
        Reply::Answer => {
            if let Err(e) = res
                && !e.kind.to_lowercase().contains("not")
            {
                app.toast(format!("✗ confirm: {}", e.message));
            }
        }
        Reply::List => {
            let p = app.gateway.per(i);
            p.list_sent = None;
            match res {
                Ok(v) => {
                    // Devices (phones/desktops) that gateways report via `client.devices`; a
                    // gateway process itself is not a device.
                    p.devices = v["devices"].as_array().map(|a| a.len() as u32).unwrap_or(0);
                    app.dirty = true;
                }
                // Older server: stop asking for a while.
                Err(_) => p.next_list = Some(now + Duration::from_secs(120)),
            }
        }
        Reply::Events => {
            app.gateway.per(i).events_sent = None;
            match res {
                Ok(v) => {
                    app.gateway.per(i).caught_up = true;
                    on_events(app, i, &v, now_ms(), now)
                }
                Err(_) => app.gateway.per(i).next_events = Some(now + Duration::from_secs(30)),
            }
        }
    }
}

/// Apply an `events.read` result (`client.confirm_requested` / `client.confirm_resolved`).
pub fn on_events(app: &mut App, i: usize, v: &Value, wall_ms: i64, now: Instant) {
    let Some(evs) = v["events"].as_array() else {
        return;
    };
    for e in evs {
        if let Some(seq) = e["seq"].as_i64() {
            let p = app.gateway.per(i);
            p.cursor = p.cursor.max(seq);
        }
        let Some(id) = e["subject"]["confirm"].as_str() else {
            continue;
        };
        match e["type"].as_str() {
            Some("client.confirm_requested") => {
                let d = &e["data"];
                let timeout = d["timeout_ms"].as_i64().unwrap_or(60_000).max(0);
                let left = timeout - (wall_ms - e["ts"].as_i64().unwrap_or(wall_ms)).max(0);
                if left <= 0 {
                    continue;
                }
                let mut options: Vec<(String, String)> = d["options"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|o| {
                                let id = o["id"].as_str()?;
                                Some((
                                    id.to_string(),
                                    o["label"].as_str().unwrap_or(id).to_string(),
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if options.is_empty() {
                    options = vec![
                        ("ok".into(), "OK".into()),
                        ("cancel".into(), "Cancel".into()),
                    ];
                }
                app.gateway.push(Confirm {
                    machine: i,
                    id: id.to_string(),
                    title: d["title"].as_str().unwrap_or("Confirm").to_string(),
                    body: d["body"].as_str().unwrap_or("").to_string(),
                    options,
                    deadline: now + Duration::from_millis(left as u64),
                    sel: 0,
                    shown_at: now,
                });
                app.dirty = true;
            }
            Some("client.confirm_resolved") => {
                app.gateway.resolve(i, id);
                app.dirty = true;
            }
            _ => {}
        }
    }
}

// ---- keys ---------------------------------------------------------------------------------

/// The overlay is modal: while open it swallows every key. True when swallowed.
pub fn key(app: &mut App, ev: &KeyEvent) -> bool {
    if !app.gateway.modal() {
        return false;
    }
    app.dirty = true;
    if let KeyOutcome::Answer {
        machine,
        id,
        choice,
    } = app.gateway.handle_key(ev, Instant::now())
    {
        app.command_on(
            machine,
            "client.confirm_answer",
            json!({"confirm": id, "choice": choice}),
            Pending::Gateway(Reply::Answer),
        );
    }
    true
}

// ---- drawing ------------------------------------------------------------------------------

/// Tab-bar indicator: `📱1` for the focused machine; other machines with devices follow as
/// `📱2@label`. None when no phone is connected anywhere.
pub fn devices_label(app: &App) -> Option<String> {
    let mut parts = Vec::new();
    for i in 0..app.machines.len() {
        let n = app.gateway.devices(i);
        if n == 0 {
            continue;
        }
        if i == app.cur || app.machines.len() == 1 {
            parts.insert(0, format!("📱{n}"));
        } else {
            parts.push(format!("📱{n}@{}", app.machines[i].label));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    for para in s.lines() {
        let mut cur = String::new();
        for word in para.split_whitespace() {
            if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > w {
                out.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
        out.push(cur);
    }
    out
}

pub fn draw_overlay(app: &App, g: &mut Grid) {
    let Some(c) = app.gateway.queue.front() else {
        return;
    };
    let t = app.theme;
    let body = wrap(&c.body, 70);
    let body_n = body.len().min(6);
    let h = (body_n + c.options.len() + 7) as u16;
    let label = &app.machines[c.machine].label;
    let mut b = crate::popups::frame(app, g, 76, h, &format!("Confirm · {label}"));
    b.line(&c.title, t.bold(t.fg));
    for l in body.iter().take(body_n) {
        b.line(l, t.text());
    }
    b.line("", t.text());
    for (i, (_, l)) in c.options.iter().enumerate() {
        let st = if i == c.sel {
            t.sel(t.accent)
        } else {
            t.text()
        };
        let mark = if i == c.sel { "▸" } else { " " };
        b.line(&format!("{mark} [{}] {l}", i + 1), st);
    }
    b.line("", t.text());
    let secs = c.remaining(Instant::now()).as_secs();
    let more = app.gateway.queue.len() - 1;
    let more = if more > 0 {
        format!(" · {more} more waiting")
    } else {
        String::new()
    };
    b.line(
        &format!("expires in {secs}s{more}"),
        if secs <= 10 { t.s(t.red) } else { t.dim() },
    );
    b.line(
        "[1-9]/letter choose · [enter] selected · [esc] dismiss (no answer)",
        t.dim(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_app;
    use crate::tasks::grid_text;
    use vk_proto::input::Mods;

    fn kev(k: Key) -> KeyEvent {
        KeyEvent::new(k, Mods::empty())
    }

    fn conf(id: &str, now: Instant, secs: u64) -> Confirm {
        Confirm {
            machine: 0,
            id: id.into(),
            title: "Pair device".into(),
            body: "Fingerprint 12-34".into(),
            options: vec![
                ("ok".into(), "Approve".into()),
                ("cancel".into(), "Cancel".into()),
            ],
            deadline: now + Duration::from_secs(secs),
            sel: 0,
            shown_at: now,
        }
    }

    #[test]
    fn activity_throttle_pings_once_per_ten_seconds() {
        let mut t = Throttle::default();
        let t0 = Instant::now();
        assert!(t.note_input(t0), "first input after idle pings");
        assert!(!t.note_input(t0 + Duration::from_secs(3)));
        assert!(!t.note_input(t0 + Duration::from_millis(9_999)));
        assert!(t.note_input(t0 + Duration::from_secs(10)));
        assert!(!t.note_input(t0 + Duration::from_secs(11)));
        assert!(t.note_input(t0 + Duration::from_secs(600)), "after idle");
    }

    #[test]
    fn input_pings_every_connected_machine_and_idle_sends_nothing() {
        let (mut app, mut rxs) = test_app(2);
        app.machines[1].tx = None;
        on_input(&mut app);
        on_input(&mut app);
        let mut n = 0;
        while let Ok(f) = rxs[0].try_recv() {
            if let vk_proto::render::ClientFrame::Command { json, .. } = f {
                assert!(json.contains("client.activity") && json.contains("last_input_ms"));
                n += 1;
            }
        }
        assert_eq!(n, 1, "throttled");
        assert!(rxs[1].try_recv().is_err(), "offline machine not pinged");
        // No input, no pings (tick only polls).
        let (mut app, mut rxs) = test_app(1);
        tick(&mut app);
        while let Ok(f) = rxs[0].try_recv() {
            if let vk_proto::render::ClientFrame::Command { json, .. } = f {
                assert!(!json.contains("client.activity"));
            }
        }
    }

    #[test]
    fn activity_reaches_all_connected_machines() {
        let (mut app, mut rxs) = test_app(2);
        on_input(&mut app);
        for rx in rxs.iter_mut() {
            match rx.try_recv() {
                Ok(vk_proto::render::ClientFrame::Command { json, .. }) => {
                    assert!(json.contains("client.activity"))
                }
                other => panic!("expected activity, got {other:?}"),
            }
        }
    }

    #[test]
    fn confirm_queue_explicit_keys_resolve_and_expiry() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(1);
        let mut s = State::default();
        s.push(conf("a", t0, 30));
        s.push(conf("a", t0, 30));
        s.push(conf("b", t0, 5));
        assert_eq!(s.queue.len(), 2, "dupes ignored, others queue");
        // Too early: typed-ahead keys never answer.
        assert_eq!(
            s.handle_key(&kev(Key::Named(NamedKey::Enter)), t0),
            KeyOutcome::None
        );
        // Ordinary typing does not answer and does not close.
        for ch in ['x', 'z', '0', '9', ' '] {
            assert_eq!(s.handle_key(&kev(Key::Char(ch)), later), KeyOutcome::None);
        }
        assert_eq!(s.queue.len(), 2);
        // Ctrl+1 is not "1", nor is any other modifier, a repeat or a release.
        for m in [Mods::CTRL, Mods::SHIFT, Mods::HYPER, Mods::META] {
            let k = KeyEvent::new(Key::Char('1'), m);
            assert_eq!(s.handle_key(&k, later), KeyOutcome::None);
        }
        for kind in [KeyKind::Repeat, KeyKind::Release] {
            let mut k = kev(Key::Char('1'));
            k.kind = kind;
            assert_eq!(s.handle_key(&k, later), KeyOutcome::None);
        }
        // Move highlight then Enter answers the highlighted button.
        s.handle_key(&kev(Key::Named(NamedKey::Right)), later);
        assert_eq!(
            s.handle_key(&kev(Key::Named(NamedKey::Enter)), later),
            KeyOutcome::Answer {
                machine: 0,
                id: "a".into(),
                choice: "cancel".into()
            }
        );
        // Next in queue: armed afresh once it is in front, then a letter picks by label initial.
        assert_eq!(s.handle_key(&kev(Key::Char('A')), later), KeyOutcome::None);
        let later = later + ARM_DELAY;
        assert_eq!(
            s.handle_key(&kev(Key::Char('A')), later),
            KeyOutcome::Answer {
                machine: 0,
                id: "b".into(),
                choice: "ok".into()
            }
        );
        assert!(!s.modal());
        // Esc dismisses without an answer; dismissed ids don't come back.
        s.push(conf("c", t0, 30));
        assert_eq!(
            s.handle_key(&kev(Key::Named(NamedKey::Escape)), later),
            KeyOutcome::Dismissed
        );
        s.push(conf("c", t0, 30));
        assert!(!s.modal());
        // Number key; resolved elsewhere; countdown expiry.
        s.push(conf("d", t0, 30));
        assert_eq!(
            s.handle_key(&kev(Key::Char('1')), later),
            KeyOutcome::Answer {
                machine: 0,
                id: "d".into(),
                choice: "ok".into()
            }
        );
        s.push(conf("e", t0, 30));
        s.resolve(0, "e");
        assert!(!s.modal());
        s.push(conf("f", t0, 5));
        assert!(!s.expire(t0 + Duration::from_secs(4)));
        assert!(s.expire(t0 + Duration::from_secs(5)));
        assert!(!s.modal());
    }

    #[test]
    fn events_open_and_close_overlay_and_swallow_keys() {
        let (mut app, mut rxs) = test_app(1);
        let now = Instant::now();
        let req = json!({"events": [{
            "seq": 7, "ts": 1_000_000, "type": "client.confirm_requested",
            "subject": {"confirm": "c1"},
            "data": {"title": "Pair?", "body": "fp", "timeout_ms": 60000,
                     "options": [{"id": "yes", "label": "Yes"}, {"id": "no", "label": "No"}]}
        }]});
        on_events(&mut app, 0, &req, 1_010_000, now);
        assert!(app.gateway.modal());
        assert_eq!(app.gateway.per(0).cursor, 7);
        // Swallowed: nothing reaches a pane.
        assert!(key(&mut app, &kev(Key::Char('q'))));
        while let Ok(f) = rxs[0].try_recv() {
            assert!(!matches!(f, vk_proto::render::ClientFrame::Key { .. }));
        }
        // Expired on arrival is ignored.
        let old = json!({"events": [{
            "seq": 8, "ts": 1, "type": "client.confirm_requested",
            "subject": {"confirm": "c2"}, "data": {"title": "x", "timeout_ms": 1000}
        }]});
        on_events(&mut app, 0, &old, 1_010_000, now);
        assert_eq!(app.gateway.queue.len(), 1);
        let res = json!({"events": [{
            "seq": 9, "ts": 1_020_000, "type": "client.confirm_resolved",
            "subject": {"confirm": "c1"}, "data": {"choice": "yes", "timed_out": false}
        }]});
        on_events(&mut app, 0, &res, 1_020_000, now);
        assert!(!app.gateway.modal());
        assert!(!key(&mut app, &kev(Key::Char('q'))), "keys flow again");
    }

    #[test]
    fn overlay_draws_in_chrome() {
        let (mut app, _rxs) = test_app(1);
        app.gateway.push(conf("a", Instant::now(), 42));
        let mut g = Grid::new(120, 40);
        crate::draw::compose(&app, &mut g);
        let s = grid_text(&g);
        for needle in [
            "Confirm · m0",
            "Pair device",
            "Fingerprint 12-34",
            "[1] Approve",
            "[2] Cancel",
            "expires in",
        ] {
            assert!(s.contains(needle), "missing {needle}:\n{s}");
        }
    }

    #[test]
    fn answered_by_formatting() {
        assert_eq!(
            answered_text("gateway:iphone"),
            Some("answered on iphone".into())
        );
        assert_eq!(
            answered_text("gateway:"),
            Some("answered on gateway:".into())
        );
        assert_eq!(answered_text("tui:abc"), None);
        let mut it = crate::app::test_interaction("i1", "p1", "Run rm", 0);
        it.answered_by = Some("gateway:the maintainer's iPhone".into());
        assert_eq!(interaction_answered_text(&it), None, "still open");
        it.status = InteractionStatus::Answered;
        assert_eq!(
            interaction_answered_text(&it).as_deref(),
            Some("answered on the maintainer's iPhone")
        );
        // Toast once.
        let (mut app, _rxs) = test_app(1);
        it.answered_at_ms = Some(now_ms());
        app.machines[0].model.interactions.push(it);
        on_model(&mut app, 0);
        on_model(&mut app, 0);
        let n = app
            .toasts
            .iter()
            .filter(|t| t.text.contains("answered on the maintainer's iPhone"))
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn devices_indicator_counts_and_renders() {
        let (mut app, _rxs) = test_app(2);
        assert_eq!(devices_label(&app), None);
        // Two gateway connections but the devices list decides (a gateway is not a device).
        let list = json!({"clients": [
            {"id": "a", "kind": "tui"}, {"id": "b", "kind": "gateway"}, {"id": "c", "kind": "gateway"}
        ], "devices": [{"name": "the maintainer's iPhone"}, {"name": "iPad"}]});
        on_reply(&mut app, 0, Reply::List, Ok(list));
        assert_eq!(app.gateway.devices(0), 2);
        assert_eq!(devices_label(&app).as_deref(), Some("📱2"));
        on_reply(
            &mut app,
            1,
            Reply::List,
            Ok(json!({"clients": [{"kind": "gateway"}], "devices": [{"name": "phone"}]})),
        );
        assert_eq!(devices_label(&app).as_deref(), Some("📱2 📱1@m1"));
        let mut g = Grid::new(120, 40);
        crate::draw::compose(&app, &mut g);
        let top: String = g.row(0).iter().map(|c| c.text.as_str()).collect();
        assert!(top.contains("📱2"), "{top}");
        on_reply(&mut app, 0, Reply::List, Ok(json!({"clients": []})));
        on_reply(&mut app, 1, Reply::List, Ok(json!({"clients": []})));
        assert_eq!(devices_label(&app), None);
    }
}
