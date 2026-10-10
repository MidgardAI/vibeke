//! The attention inbox (08 §6.6 evolved in place per 15 §8): one surface under `prefix+i` that
//! replaces the pane area until dismissed. It merges `attention.list` from every connected
//! machine (falling back to the M1 ordering built from the session model when a server doesn't
//! have the method yet), keeps the selection on the same object while the list reorders, and
//! offers the five-minute view, snooze and effort. Nothing here takes focus or writes PTY bytes
//! except an explicit Open pane / answer.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::draw::{harness_icon, truncate};
use crate::screen::{Grid, Rect as SRect};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use vk_proto::input::{Key, KeyEvent, NamedKey};
use vk_proto::model::*;
use vk_proto::render::Style;

/// Refresh at most this often while the inbox is open.
pub const REFRESH_MIN: Duration = Duration::from_secs(2);
pub const FIVE_MINUTES_MS: i64 = 300_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ItemKey {
    pub machine: usize,
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub key: ItemKey,
    /// 1–4 per 15 §8.1; 5 = finished turns footer.
    pub class: u8,
    pub title: String,
    pub subtitle: String,
    pub task: Option<String>,
    pub run: Option<String>,
    pub pane: Option<String>,
    pub interaction: Option<String>,
    pub explanation: String,
    pub age_ms: i64,
    pub risk: Option<String>,
    pub effort: Option<String>,
    /// Open tasks transitively waiting for this item's task through confirmed `blocks` links
    /// (15 §8.1, T4).
    pub blocks_tasks: u64,
    /// `effort_estimate {effort, source}` when the user hasn't set effort (T4): (effort, source).
    pub effort_estimate: Option<(String, String)>,
    pub snoozed_until_ms: Option<i64>,
    pub woke_from_snooze: Option<String>,
    pub urgent: bool,
    /// The server's key object, echoed back in `attention.update`.
    pub raw_key: Value,
    /// Built locally from the session model (server lacks `attention.list`).
    pub fallback: bool,
    /// Last observed on a machine that is offline now: shown, never actionable.
    pub stale: bool,
    /// The interaction's deadline (15 §8.1): epoch ms and source (`native` = the harness's
    /// own timeout, `gate` = Vibeke's hook gate ends, the pane dialog shows afterwards).
    pub deadline_ms: Option<i64>,
    pub deadline_source: Option<String>,
    /// Server-side batch of equivalent approvals (15 §8.3): (batch id, size).
    pub batch: Option<(String, u64)>,
}

/// A busy agent without an open question, for the **Also working** footer (15 §8.1).
#[derive(Debug, Clone, PartialEq)]
pub struct Working {
    pub machine: usize,
    pub run: String,
    pub pane: String,
    pub name: String,
    pub task: Option<String>,
    pub working_for_ms: i64,
}

impl Item {
    /// Hidden by a snooze right now. The server's latest word wins: an item it returned with
    /// `woke_from_snooze` (a material change, deadline or escalation woke it, 15 §8.3) is shown
    /// even if its old snooze deadline is still in the future.
    pub fn snoozed(&self, now: i64) -> bool {
        self.woke_from_snooze.is_none() && self.snoozed_until_ms.is_some_and(|u| u > now)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Five {
    pub keys: Vec<(String, String)>,
    pub omitted_count: usize,
    pub note: String,
}

#[derive(Debug, Clone, Default)]
pub enum Source {
    #[default]
    Unknown,
    /// `method_not_found`: use the M1 inbox for this machine until it reconnects.
    Unsupported,
    Loaded {
        items: Vec<Item>,
        complete: bool,
        notes: Vec<String>,
        five: Option<Five>,
        at: Instant,
        at_ms: i64,
        also_working: Vec<Working>,
    },
    Error(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Sub {
    None,
    Snooze(SnoozeChooser),
    Effort { sel: usize },
}

#[derive(Debug, Clone)]
pub struct InboxState {
    pub per_machine: Vec<Source>,
    pub five_minute: bool,
    pub selected: Option<ItemKey>,
    pub sel_idx: usize,
    /// Keys seen since the inbox opened; anything new and urgent gets an indicator instead of
    /// moving the selection.
    pub seen_keys: HashSet<ItemKey>,
    pub fresh: HashSet<ItemKey>,
    pub sub: Sub,
    pub last_fetch: Option<Instant>,
    pub stale: bool,
    pub outstanding: HashSet<usize>,
    /// `prefix+a` waiting for fresh rankings (`true`: focus the pane instead of the card).
    pub next_after: Option<bool>,
    pub notice: Option<String>,
}

impl Default for InboxState {
    fn default() -> Self {
        InboxState {
            per_machine: Vec::new(),
            five_minute: false,
            selected: None,
            sel_idx: 0,
            seen_keys: HashSet::new(),
            fresh: HashSet::new(),
            sub: Sub::None,
            last_fetch: None,
            stale: false,
            outstanding: HashSet::new(),
            next_after: None,
            notice: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Reply {
    List { budget: bool },
    Updated,
    EffortSet,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- parsing ------------------------------------------------------------------------------------

fn class_of(v: &Value) -> u8 {
    match v {
        Value::Number(n) => n.as_u64().unwrap_or(4).clamp(1, 4) as u8,
        Value::String(s) if s == "finished_turns" => 5,
        Value::String(s) => s.parse::<u8>().unwrap_or(4).clamp(1, 5),
        _ => 4,
    }
}

fn opt_str(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

pub fn parse_item(machine: usize, v: &Value) -> Option<Item> {
    let key = v.get("key")?;
    let kind = key.get("kind")?.as_str()?.to_string();
    let id = match key.get("id")? {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    Some(Item {
        key: ItemKey { machine, kind, id },
        class: class_of(v.get("class").unwrap_or(&Value::Null)),
        title: opt_str(v, "title").unwrap_or_default(),
        subtitle: opt_str(v, "subtitle").unwrap_or_default(),
        task: opt_str(v, "task"),
        run: opt_str(v, "run"),
        pane: opt_str(v, "pane"),
        interaction: opt_str(v, "interaction"),
        explanation: opt_str(v, "explanation").unwrap_or_default(),
        age_ms: v.get("age_ms").and_then(Value::as_i64).unwrap_or(0),
        risk: opt_str(v, "risk"),
        effort: opt_str(v, "effort"),
        blocks_tasks: v.get("blocks_tasks").and_then(Value::as_u64).unwrap_or(0),
        effort_estimate: v.get("effort_estimate").and_then(|e| {
            let eff = e.get("effort")?.as_str()?.to_string();
            let src = e
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("heuristic")
                .to_string();
            Some((eff, src))
        }),
        snoozed_until_ms: v.get("snoozed_until_ms").and_then(Value::as_i64),
        woke_from_snooze: opt_str(v, "woke_from_snooze"),
        urgent: v.get("urgent").and_then(Value::as_bool).unwrap_or(false),
        raw_key: key.clone(),
        fallback: false,
        stale: false,
        deadline_ms: v.get("deadline_ms").and_then(Value::as_i64),
        deadline_source: opt_str(v, "deadline_source"),
        batch: v.get("batch").and_then(|b| {
            Some((
                b.get("id")?.as_str()?.to_string(),
                b.get("size").and_then(Value::as_u64).unwrap_or(0),
            ))
        }),
    })
}

/// Parse an `attention.list` result.
pub fn parse_list(machine: usize, v: &Value, at: Instant, at_ms: i64) -> Source {
    let items = v
        .get("items")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| parse_item(machine, x)).collect())
        .unwrap_or_default();
    let cov = v.get("coverage").cloned().unwrap_or(Value::Null);
    let complete = cov.get("complete").and_then(Value::as_bool).unwrap_or(true);
    let notes = cov
        .get("notes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|n| n.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let five = v.get("five_minute").filter(|f| !f.is_null()).map(|f| Five {
        keys: f
            .get("keys")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|k| {
                        let kind = k.get("kind")?.as_str()?.to_string();
                        let id = match k.get("id")? {
                            Value::String(s) => s.clone(),
                            o => o.to_string(),
                        };
                        Some((kind, id))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        omitted_count: f.get("omitted_count").and_then(Value::as_u64).unwrap_or(0) as usize,
        note: opt_str(f, "note").unwrap_or_default(),
    });
    let also_working = v
        .get("also_working")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|w| {
                    Some(Working {
                        machine,
                        run: w.get("run")?.as_str()?.to_string(),
                        pane: opt_str(w, "pane").unwrap_or_default(),
                        name: opt_str(w, "name").unwrap_or_default(),
                        task: w
                            .get("task")
                            .and_then(|t| t.get("title"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        working_for_ms: w
                            .get("working_for_ms")
                            .and_then(Value::as_i64)
                            .unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Source::Loaded {
        items,
        complete,
        notes,
        five,
        at,
        at_ms,
        also_working,
    }
}

/// Busy agents without an open question from the session model (older servers, fallback).
pub fn working_from_model(app: &App, mi: usize) -> Vec<Working> {
    let m = &app.machines[mi];
    let now = now_ms();
    let mut v: Vec<Working> = m
        .model
        .runs
        .iter()
        .filter(|r| r.ended_at_ms.is_none() && r.execution.value == Execution::Working)
        .filter(|r| {
            !m.model
                .interactions
                .iter()
                .any(|i| i.run == r.id && i.status == InteractionStatus::Open)
        })
        .map(|r| Working {
            machine: mi,
            run: r.id.clone(),
            pane: r.pane.clone(),
            name: r.label().to_string(),
            task: None,
            working_for_ms: (now - r.execution.since_ms).max(0),
        })
        .collect();
    v.sort_by_key(|w| std::cmp::Reverse(w.working_for_ms));
    v
}

// ---- M1 fallback --------------------------------------------------------------------------------

/// Today's inbox for one machine, built from the session model: open interactions on agents the
/// user isn't looking at (oldest first), then tasks with a review available.
pub fn fallback_items(app: &App, mi: usize) -> Vec<Item> {
    let m = &app.machines[mi];
    let focused = app.focused_pane();
    let now = now_ms();
    let mut v = Vec::new();
    let mut ints: Vec<&Interaction> = m
        .model
        .interactions
        .iter()
        .filter(|i| {
            i.status == InteractionStatus::Open
                && !(mi == app.cur && Some(&i.pane) == focused.as_ref())
        })
        .collect();
    ints.sort_by_key(|i| i.opened_at_ms);
    for i in ints {
        let run = m.model.runs.iter().find(|r| r.id == i.run);
        let who = run
            .map(|r| format!("{} {}", harness_icon(&r.harness), r.label()))
            .unwrap_or_default();
        let age = (now - i.opened_at_ms).max(0);
        v.push(Item {
            key: ItemKey {
                machine: mi,
                kind: "interaction".into(),
                id: i.id.clone(),
            },
            class: 3,
            title: i.title.clone(),
            subtitle: format!("{who} · {}", i.kind.as_str()),
            task: None,
            run: Some(i.run.clone()),
            pane: Some(i.pane.clone()),
            interaction: Some(i.id.clone()),
            explanation: format!("Waiting {}", fmt_age(age)),
            age_ms: age,
            risk: i.action.as_ref().map(|a| {
                match a.risk {
                    Risk::High => "high",
                    Risk::Medium => "medium",
                    Risk::Low => "low",
                    Risk::Unknown => "unknown",
                }
                .to_string()
            }),
            effort: None,
            blocks_tasks: 0,
            effort_estimate: None,
            snoozed_until_ms: None,
            woke_from_snooze: None,
            urgent: false,
            raw_key: json!({"kind": "interaction", "id": i.id}),
            fallback: true,
            stale: false,
            deadline_ms: None,
            deadline_source: None,
            batch: None,
        });
    }
    for t in &m.model.tasks {
        let Some(l) = t.review_label.as_deref() else {
            continue;
        };
        if !matches!(
            l,
            "review_available" | "ready_for_review" | "review_outdated"
        ) || t.status != "active"
        {
            continue;
        }
        let age = (now - t.created_at_ms).max(0);
        v.push(Item {
            key: ItemKey {
                machine: mi,
                kind: "review".into(),
                id: t.id.clone(),
            },
            class: 4,
            title: t.title.clone(),
            subtitle: format!("#{} · {}", t.handle, crate::tasks::label_text(l)),
            task: Some(t.id.clone()),
            run: None,
            pane: None,
            interaction: None,
            explanation: crate::tasks::label_text(l).to_string(),
            age_ms: age,
            risk: None,
            effort: t.effort.clone(),
            blocks_tasks: 0,
            effort_estimate: None,
            snoozed_until_ms: None,
            woke_from_snooze: None,
            urgent: false,
            raw_key: json!({"kind": "review", "id": t.id}),
            fallback: true,
            stale: false,
            deadline_ms: None,
            deadline_source: None,
            batch: None,
        });
    }
    v
}

pub fn fmt_age(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

// ---- merged view --------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct View {
    pub items: Vec<Item>,
    pub coverage: Vec<String>,
    /// Five-minute view: (omitted count, note).
    pub omitted: Option<(usize, String)>,
    pub snoozed: usize,
    /// The **Also working** footer (15 §8.1; `ui.inbox.also_working`).
    pub also_working: Vec<Working>,
}

fn rank_key(i: &Item) -> (u8, bool, std::cmp::Reverse<i64>) {
    (i.class, !i.urgent, std::cmp::Reverse(i.age_ms))
}

/// k-way merge: each machine's list keeps the server's order; heads are compared by class,
/// urgency and age so a multi-machine inbox still reads top-down by precedence.
pub fn merge(lists: Vec<Vec<Item>>) -> Vec<Item> {
    let mut heads: Vec<std::collections::VecDeque<Item>> =
        lists.into_iter().map(|l| l.into_iter().collect()).collect();
    let mut out = Vec::new();
    loop {
        let best = heads
            .iter()
            .enumerate()
            .filter_map(|(i, q)| q.front().map(|it| (i, rank_key(it))))
            .min_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        match best {
            Some((i, _)) => out.push(heads[i].pop_front().unwrap()),
            None => return out,
        }
    }
}

/// The merged inbox across machines (with honest coverage lines).
pub fn view(app: &App) -> View {
    let st = &app.inbox;
    let now = now_ms();
    let multi = app.machines.len() > 1;
    let mut lists = Vec::new();
    let mut coverage = Vec::new();
    let mut omitted: Option<(usize, String)> = None;
    let mut snoozed = 0;
    let mut working: Vec<Working> = Vec::new();
    for (mi, m) in app.machines.iter().enumerate() {
        let src = st.per_machine.get(mi).cloned().unwrap_or_default();
        let label = if m.label.is_empty() {
            "local".to_string()
        } else {
            m.label.clone()
        };
        match src {
            Source::Loaded {
                items,
                complete,
                notes,
                five,
                at,
                also_working,
                ..
            } => {
                let elapsed = at.elapsed().as_millis() as i64;
                let offline = !m.connected();
                if !offline {
                    working.extend(also_working.into_iter().map(|mut w| {
                        w.working_for_ms += elapsed;
                        w
                    }));
                }
                if offline {
                    coverage.push(format!(
                        "{label} offline · last observed {} ago · actions disabled",
                        fmt_age(elapsed)
                    ));
                }
                if !complete {
                    if notes.is_empty() {
                        coverage.push(format!("{label}: coverage incomplete"));
                    }
                    for n in &notes {
                        coverage.push(if multi {
                            format!("{label}: {n}")
                        } else {
                            n.clone()
                        });
                    }
                }
                let mut v: Vec<Item> = items
                    .into_iter()
                    .map(|mut i| {
                        i.age_ms += elapsed;
                        i.stale = offline;
                        i
                    })
                    .collect();
                let before = v.len();
                v.retain(|i| !i.snoozed(now) || i.urgent);
                snoozed += before - v.len();
                if st.five_minute
                    && let Some(f) = &five
                {
                    v.retain(|i| {
                        i.urgent
                            || f.keys
                                .iter()
                                .any(|(k, id)| *k == i.key.kind && *id == i.key.id)
                    });
                    let o = omitted.get_or_insert((0, String::new()));
                    o.0 += f.omitted_count;
                    if o.1.is_empty() {
                        o.1 = f.note.clone();
                    }
                }
                lists.push(v);
            }
            Source::Unsupported => {
                if m.connected() {
                    coverage.push(format!(
                        "{label}: ranking unavailable (older server) — open questions in arrival order"
                    ));
                    lists.push(fallback_items(app, mi));
                    working.extend(working_from_model(app, mi));
                } else {
                    coverage.push(format!("{label} offline — not covered"));
                }
            }
            Source::Error(e) => {
                coverage.push(format!("{label}: {e} — showing open questions only"));
                if m.connected() {
                    lists.push(fallback_items(app, mi));
                    working.extend(working_from_model(app, mi));
                }
            }
            Source::Unknown => {
                if m.connected() {
                    // Not answered yet: show what the session model knows without inventing ranks.
                    lists.push(fallback_items(app, mi));
                    working.extend(working_from_model(app, mi));
                } else {
                    coverage.push(format!("{label} {} — not covered", m.status));
                }
            }
        }
    }
    // Incoming handoffs waiting to be accepted (16 §15.2).
    lists.push(crate::handoff::inbox_items(app));
    if st.five_minute && omitted.is_none() {
        coverage.push("five-minute view needs a newer server; showing all items".into());
    }
    if !app.config.ui.inbox.also_working {
        working.clear();
    }
    View {
        items: merge(lists),
        coverage,
        omitted,
        snoozed,
        also_working: working,
    }
}

/// Keep the selection on the same object while the list reorders; if it disappeared, select the
/// item now at the same position and say so.
pub fn sync_selection(st: &mut InboxState, items: &[Item]) {
    let keys: HashSet<ItemKey> = items.iter().map(|i| i.key.clone()).collect();
    for i in items {
        if !st.seen_keys.contains(&i.key) {
            if !st.seen_keys.is_empty() && (i.urgent || i.class <= 2) {
                st.fresh.insert(i.key.clone());
            }
            st.seen_keys.insert(i.key.clone());
        }
    }
    st.fresh.retain(|k| keys.contains(k));
    match &st.selected {
        Some(k) => {
            if let Some(p) = items.iter().position(|i| &i.key == k) {
                st.sel_idx = p;
            } else {
                st.notice = Some("The selected item no longer needs you".into());
                st.sel_idx = st.sel_idx.min(items.len().saturating_sub(1));
                st.selected = items.get(st.sel_idx).map(|i| i.key.clone());
                st.sub = Sub::None;
            }
        }
        None => {
            st.sel_idx = st.sel_idx.min(items.len().saturating_sub(1));
            st.selected = items.get(st.sel_idx).map(|i| i.key.clone());
        }
    }
}

// ---- fetching -----------------------------------------------------------------------------------

pub fn open(app: &mut App) {
    app.inbox.seen_keys.clear();
    app.inbox.fresh.clear();
    app.inbox.notice = None;
    app.inbox.sub = Sub::None;
    app.mode = Mode::Popup(Popup::Inbox);
    // The selection is kept from the last time (if that item still exists) once rankings
    // arrive; selecting a provisional model-built item now could report it as "resolved".
    refresh(app);
}

/// Ask every connected machine that may support it for a fresh ranking.
pub fn refresh(app: &mut App) {
    let n = app.machines.len();
    if app.inbox.per_machine.len() < n {
        app.inbox.per_machine.resize(n, Source::Unknown);
    }
    app.inbox.last_fetch = Some(Instant::now());
    app.inbox.stale = false;
    let budget = app.inbox.five_minute;
    for mi in 0..n {
        if !app.machines[mi].connected() || matches!(app.inbox.per_machine[mi], Source::Unsupported)
        {
            continue;
        }
        let params = if budget {
            json!({"budget_ms": FIVE_MINUTES_MS})
        } else {
            json!({})
        };
        app.inbox.outstanding.insert(mi);
        app.command_on(
            mi,
            "attention.list",
            params,
            Pending::Attn(Reply::List { budget }),
        );
    }
}

/// A model change or event: refresh soon (rate-limited in `tick`).
pub fn invalidate(app: &mut App) {
    app.inbox.stale = true;
}

/// While the inbox is open and stale: the next refresh (at most every `REFRESH_MIN`).
pub(crate) fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    if matches!(app.mode, Mode::Popup(Popup::Inbox)) && app.inbox.stale {
        d.at(
            "inbox",
            app.inbox.last_fetch.map_or(now, |t| t + REFRESH_MIN),
        );
    }
}

pub fn tick(app: &mut App) {
    let open = matches!(app.mode, Mode::Popup(Popup::Inbox));
    if open
        && app.inbox.stale
        && app
            .inbox
            .last_fetch
            .is_none_or(|t| t.elapsed() >= REFRESH_MIN)
    {
        refresh(app);
    }
}

/// A machine (re)connected: it may have been upgraded, so ask again.
pub fn on_connected(app: &mut App, mi: usize) {
    if let Some(s) = app.inbox.per_machine.get_mut(mi)
        && matches!(s, Source::Unsupported | Source::Error(_))
    {
        *s = Source::Unknown;
    }
    app.inbox.stale = true;
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    if app.inbox.per_machine.len() < app.machines.len() {
        app.inbox
            .per_machine
            .resize(app.machines.len(), Source::Unknown);
    }
    match r {
        Reply::List { budget } => {
            app.inbox.outstanding.remove(&mi);
            match res {
                Ok(v) => {
                    let mut src = parse_list(mi, &v, Instant::now(), now_ms());
                    if budget
                        && let Source::Loaded { five, .. } = &mut src
                        && five.is_none()
                    {
                        // Asked for a budget but the server ignored it: All items only.
                        *five = None;
                    }
                    app.inbox.per_machine[mi] = src;
                }
                Err(e) if e.is_method_not_found() => {
                    app.inbox.per_machine[mi] = Source::Unsupported;
                }
                Err(e) => app.inbox.per_machine[mi] = Source::Error(e.message),
            }
            let v = view(app);
            sync_selection(&mut app.inbox, &v.items);
            if app.inbox.outstanding.is_empty()
                && let Some(focus) = app.inbox.next_after.take()
            {
                act_on_first(app, focus);
            }
        }
        Reply::Updated | Reply::EffortSet => match res {
            Ok(_) => {
                app.inbox.stale = true;
                app.inbox.last_fetch = None;
            }
            Err(e) if e.is_method_not_found() => {
                app.toast("snooze needs a newer server on that machine");
            }
            Err(e) => app.toast(format!("✗ {}", e.message)),
        },
    }
    app.dirty = true;
}

// ---- prefix+a ------------------------------------------------------------------------------------

/// `next_attention` follows the `attention.list` ranking; machines without it keep the M1 rule.
pub fn next_attention(app: &mut App, focus: bool) {
    let n = app.machines.len();
    if app.inbox.per_machine.len() < n {
        app.inbox.per_machine.resize(n, Source::Unknown);
    }
    let any_ranked = (0..n).any(|mi| {
        app.machines[mi].connected() && !matches!(app.inbox.per_machine[mi], Source::Unsupported)
    });
    if !any_ranked {
        app.next_attention_m1(focus);
        return;
    }
    let fresh = app
        .inbox
        .last_fetch
        .is_some_and(|t| t.elapsed() < REFRESH_MIN)
        && !app.inbox.stale
        && app.inbox.outstanding.is_empty()
        && !app.inbox.five_minute;
    if fresh {
        act_on_first(app, focus);
        return;
    }
    let was_five = app.inbox.five_minute;
    app.inbox.five_minute = false;
    app.inbox.next_after = Some(focus);
    refresh(app);
    app.inbox.five_minute = was_five;
    if app.inbox.outstanding.is_empty()
        && let Some(f) = app.inbox.next_after.take()
    {
        act_on_first(app, f);
    }
}

fn act_on_first(app: &mut App, focus: bool) {
    let was_five = app.inbox.five_minute;
    app.inbox.five_minute = false;
    let v = view(app);
    app.inbox.five_minute = was_five;
    let now = now_ms();
    let first = v.items.into_iter().find(|i| !i.stale && !i.snoozed(now));
    let Some(it) = first else {
        // Nothing ranked: the M1 fallback still focuses an unseen finished turn.
        app.next_attention_m1(focus);
        return;
    };
    open_item(app, &it, focus);
}

/// Open what an item is about. Only explicit user actions get here.
pub fn open_item(app: &mut App, it: &Item, focus: bool) {
    let mi = it.key.machine;
    if it.key.kind == "handoff" {
        crate::handoff::open_accept(app, mi, &it.key.id);
        return;
    }
    if let Some(int) = it
        .interaction
        .clone()
        .or_else(|| (it.key.kind == "interaction").then(|| it.key.id.clone()))
        && app.machines[mi]
            .model
            .interactions
            .iter()
            .any(|x| x.id == int && x.status == InteractionStatus::Open)
    {
        if focus {
            if let Some(p) = it.pane.clone().or_else(|| {
                app.machines[mi]
                    .model
                    .interactions
                    .iter()
                    .find(|x| x.id == int)
                    .map(|x| x.pane.clone())
            }) {
                app.focus_pane(mi, &p);
                app.mode = Mode::Normal;
            }
        } else {
            // `ui.interaction_overlay` decides (08 §8).
            crate::popup_pane::open_card(app, mi, &int);
        }
        return;
    }
    if let Some(t) = it.task.clone() {
        crate::tasks::open_task(app, mi, &t);
        return;
    }
    if let Some(p) = it.pane.clone() {
        app.focus_pane(mi, &p);
        app.mode = Mode::Normal;
        return;
    }
    app.toast("nothing to open for this item");
}

// ---- snooze ---------------------------------------------------------------------------------------

pub const SNOOZE_CHOICES: [&str; 4] = ["15 minutes", "1 hour", "Until tomorrow 9:00", "Custom…"];

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SnoozeChooser {
    pub sel: usize,
    /// `Some` while typing a custom time ("45m", "2h", "17:30").
    pub custom: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SnoozeOutcome {
    Stay,
    Cancel,
    Until(i64),
}

impl SnoozeChooser {
    pub fn key(&mut self, ev: &KeyEvent, now_ms: i64) -> SnoozeOutcome {
        if let Some(buf) = &mut self.custom {
            match ev.key {
                Key::Named(NamedKey::Escape) => {
                    self.custom = None;
                    self.error = None;
                }
                Key::Named(NamedKey::Backspace) => {
                    buf.pop();
                }
                Key::Named(NamedKey::Enter) => match parse_custom(buf, now_ms) {
                    Some(t) => return SnoozeOutcome::Until(t),
                    None => self.error = Some("use 45m, 2h or HH:MM".into()),
                },
                Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => buf.push(c),
                _ => {}
            }
            return SnoozeOutcome::Stay;
        }
        match ev.key {
            Key::Named(NamedKey::Escape) => SnoozeOutcome::Cancel,
            Key::Char('j') | Key::Named(NamedKey::Down) => {
                self.sel = (self.sel + 1).min(SNOOZE_CHOICES.len() - 1);
                SnoozeOutcome::Stay
            }
            Key::Char('k') | Key::Named(NamedKey::Up) => {
                self.sel = self.sel.saturating_sub(1);
                SnoozeOutcome::Stay
            }
            Key::Char(c @ '1'..='4') => {
                self.sel = (c as u8 - b'1') as usize;
                self.choose(now_ms)
            }
            Key::Named(NamedKey::Enter) => self.choose(now_ms),
            _ => SnoozeOutcome::Stay,
        }
    }

    fn choose(&mut self, now_ms: i64) -> SnoozeOutcome {
        match self.sel {
            0 => SnoozeOutcome::Until(now_ms + 15 * 60_000),
            1 => SnoozeOutcome::Until(now_ms + 3_600_000),
            2 => SnoozeOutcome::Until(local_at(now_ms, 1, 9, 0)),
            _ => {
                self.custom = Some(String::new());
                SnoozeOutcome::Stay
            }
        }
    }
}

/// `45m`, `2h`, `90` (minutes) or `HH:MM` (today if still ahead, else tomorrow).
pub fn parse_custom(s: &str, now_ms: i64) -> Option<i64> {
    let s = s.trim();
    if let Some((h, m)) = s.split_once(':') {
        let h: i64 = h.trim().parse().ok()?;
        let m: i64 = m.trim().parse().ok()?;
        if !(0..24).contains(&h) || !(0..60).contains(&m) {
            return None;
        }
        let today = local_at(now_ms, 0, h as i32, m as i32);
        return Some(if today > now_ms {
            today
        } else {
            local_at(now_ms, 1, h as i32, m as i32)
        });
    }
    let (num, mult) = if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000)
    } else {
        (s, 60_000)
    };
    let n: i64 = num.trim().parse().ok()?;
    (n > 0 && n <= 60 * 24 * 14).then_some(now_ms + n * mult)
}

/// Local wall-clock time `days` days after `now_ms` at `hh:mm`.
pub fn local_at(now_ms: i64, days: i32, hh: i32, mm: i32) -> i64 {
    let t = (now_ms / 1000) as libc::time_t;
    // SAFETY: localtime_r/mktime with valid pointers to stack values.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return now_ms + days as i64 * 86_400_000;
        }
        tm.tm_mday += days;
        tm.tm_hour = hh;
        tm.tm_min = mm;
        tm.tm_sec = 0;
        tm.tm_isdst = -1;
        let r = libc::mktime(&mut tm);
        if r == -1 {
            return now_ms + days as i64 * 86_400_000;
        }
        r as i64 * 1000
    }
}

pub const EFFORTS: [(&str, &str); 4] = [
    ("quick", "quick"),
    ("minutes", "a few minutes"),
    ("deep", "deep review"),
    ("unknown", "unknown"),
];

// ---- keys ------------------------------------------------------------------------------------------

pub fn key(app: &mut App, ev: KeyEvent) {
    let v = view(app);
    sync_selection(&mut app.inbox, &v.items);
    let sel = v.items.get(app.inbox.sel_idx).cloned();
    app.mode = Mode::Popup(Popup::Inbox);
    // Sub-choosers first.
    match app.inbox.sub.clone() {
        Sub::Snooze(mut ch) => {
            match ch.key(&ev, now_ms()) {
                SnoozeOutcome::Stay => app.inbox.sub = Sub::Snooze(ch),
                SnoozeOutcome::Cancel => app.inbox.sub = Sub::None,
                SnoozeOutcome::Until(t) => {
                    app.inbox.sub = Sub::None;
                    if let Some(it) = &sel {
                        let mi = it.key.machine;
                        app.command_on(
                            mi,
                            "attention.update",
                            json!({"key": it.raw_key, "snooze_until_ms": t}),
                            Pending::Attn(Reply::Updated),
                        );
                        app.inbox.notice = Some(format!(
                            "Snoozed “{}” for {} — the source is not resolved",
                            truncate(&it.title, 40),
                            fmt_age(t - now_ms())
                        ));
                    }
                }
            }
            return;
        }
        Sub::Effort { sel: es } => {
            match ev.key {
                Key::Named(NamedKey::Escape) => app.inbox.sub = Sub::None,
                Key::Char('j') | Key::Named(NamedKey::Down) => {
                    app.inbox.sub = Sub::Effort {
                        sel: (es + 1).min(EFFORTS.len() - 1),
                    }
                }
                Key::Char('k') | Key::Named(NamedKey::Up) => {
                    app.inbox.sub = Sub::Effort {
                        sel: es.saturating_sub(1),
                    }
                }
                Key::Named(NamedKey::Enter) => {
                    app.inbox.sub = Sub::None;
                    if let Some(it) = &sel
                        && let Some(t) = &it.task
                    {
                        app.command_on(
                            it.key.machine,
                            "task.set",
                            json!({"task": t, "effort": EFFORTS[es].0}),
                            Pending::Attn(Reply::EffortSet),
                        );
                    }
                }
                _ => {}
            }
            return;
        }
        Sub::None => {}
    }
    let n = v.items.len();
    let move_to = |app: &mut App, idx: usize| {
        app.inbox.sel_idx = idx;
        app.inbox.selected = v.items.get(idx).map(|i| i.key.clone());
        app.inbox.notice = None;
        if let Some(k) = &app.inbox.selected {
            app.inbox.fresh.remove(k);
        }
    };
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => app.mode = Mode::Normal,
        Key::Char('j') | Key::Named(NamedKey::Down) => {
            move_to(app, (app.inbox.sel_idx + 1).min(n.saturating_sub(1)))
        }
        Key::Char('k') | Key::Named(NamedKey::Up) => {
            move_to(app, app.inbox.sel_idx.saturating_sub(1))
        }
        Key::Char('g') | Key::Named(NamedKey::Home) => move_to(app, 0),
        Key::Char('G') | Key::Named(NamedKey::End) => move_to(app, n.saturating_sub(1)),
        Key::Char('f') => {
            app.inbox.five_minute = !app.inbox.five_minute;
            refresh(app);
        }
        Key::Char('r') => refresh(app),
        _ if sel.is_none() => {}
        // Batch view seeded with the selected approval (15 §8.3).
        Key::Char('A') => {
            let it = sel.unwrap();
            match (&it.interaction, it.stale) {
                (Some(int), false) => {
                    app.return_to.push(Popup::Inbox);
                    crate::batch::open(app, Some((it.key.machine, int.clone())));
                    if matches!(app.mode, Mode::Popup(Popup::Inbox)) {
                        app.return_to.pop();
                    }
                }
                _ => app.inbox.notice = Some("only approvals can be batched".into()),
            }
        }
        Key::Char('s') => {
            let it = sel.unwrap();
            if it.key.kind == "handoff" {
                app.inbox.notice =
                    Some("a handoff waits until it is accepted, declined or expires".into());
            } else if it.stale || it.fallback {
                app.inbox.notice = Some(if it.stale {
                    "machine offline — snooze when it reconnects".into()
                } else {
                    "snooze needs a newer server on that machine".into()
                });
            } else {
                app.inbox.sub = Sub::Snooze(SnoozeChooser::default());
            }
        }
        Key::Char('e') => {
            let it = sel.unwrap();
            if it.task.is_none() {
                app.inbox.notice = Some("effort applies to tracked tasks".into());
            } else if it.stale {
                app.inbox.notice = Some("machine offline — actions disabled".into());
            } else {
                let cur = EFFORTS
                    .iter()
                    .position(|(v, _)| it.effort.as_deref() == Some(*v))
                    .unwrap_or(3);
                app.inbox.sub = Sub::Effort { sel: cur };
            }
        }
        Key::Char('o') => {
            let it = sel.unwrap();
            let pane = it.pane.clone().or_else(|| {
                it.interaction.as_ref().and_then(|id| {
                    app.machines[it.key.machine]
                        .model
                        .interactions
                        .iter()
                        .find(|x| &x.id == id)
                        .map(|x| x.pane.clone())
                })
            });
            match pane {
                Some(p) if !it.stale => {
                    // Explicit Open pane: the only path from the inbox that moves focus.
                    app.focus_pane(it.key.machine, &p);
                    app.mode = Mode::Normal;
                }
                _ => app.inbox.notice = Some("no pane to open".into()),
            }
        }
        Key::Named(NamedKey::Enter) => {
            let it = sel.unwrap();
            if it.stale {
                app.inbox.notice = Some("machine offline — actions disabled".into());
                return;
            }
            app.return_to.push(Popup::Inbox);
            open_item(app, &it, false);
            if matches!(app.mode, Mode::Popup(Popup::Inbox)) {
                app.return_to.pop();
            }
        }
        // Quick answers for the selected interaction reuse the card's delivery path.
        Key::Char('y' | 'n') | Key::Char('1'..='9') => {
            let it = sel.unwrap();
            if it.stale {
                return;
            }
            let Some(int) = it.interaction.clone() else {
                return;
            };
            let mi = it.key.machine;
            let Some(x) = app.machines[mi]
                .model
                .interactions
                .iter()
                .find(|x| x.id == int)
                .cloned()
            else {
                return;
            };
            if x.status != InteractionStatus::Open || !x.answerable {
                app.inbox.notice = Some("answer this one in the pane ([o] open pane)".into());
                return;
            }
            match (x.kind, ev.key) {
                (InteractionKind::Approval | InteractionKind::PlanReview, Key::Char('y')) => {
                    app.answer(mi, &x.id, json!({"decision": "allow"}))
                }
                (InteractionKind::Approval | InteractionKind::PlanReview, Key::Char('n')) => {
                    app.answer(mi, &x.id, json!({"decision": "deny"}))
                }
                (InteractionKind::Question | InteractionKind::Picker, Key::Char(c))
                    if c.is_ascii_digit() =>
                {
                    let idx = c.to_digit(10).unwrap_or(1).saturating_sub(1) as usize;
                    if let Some(a) = crate::popups::choice_answer(&x, idx) {
                        app.answer(mi, &x.id, a);
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

// ---- drawing ---------------------------------------------------------------------------------------

fn class_glyph(i: &Item) -> &'static str {
    match i.class {
        1 => "‼",
        2 => "⏱",
        3 => match i.key.kind.as_str() {
            "interaction" => "?",
            "handoff" => "⇣",
            _ => "!",
        },
        4 => "◆",
        _ => "✓",
    }
}

pub fn draw(app: &App, g: &mut Grid) {
    let t = app.theme;
    let area = app.pane_area();
    let r = SRect {
        x: area.x,
        y: area.y,
        w: area.w,
        h: area.h,
    };
    g.fill(r, t.text());
    let v = view(app);
    let st = &app.inbox;
    let sel_idx = st
        .selected
        .as_ref()
        .and_then(|k| v.items.iter().position(|i| &i.key == k))
        .unwrap_or(st.sel_idx.min(v.items.len().saturating_sub(1)));
    let actionable = v.items.iter().filter(|i| i.class <= 4).count();
    let header = if st.five_minute {
        format!("Needs you · {actionable}   [5-minute view]  f: All items")
    } else {
        format!("Needs you · {actionable}   f: 5-minute view")
    };
    g.put_str(
        r.x + 1,
        r.y,
        &header,
        t.bold(t.accent),
        r.w.saturating_sub(2),
    );
    let mut y = r.y + 1;
    if let Some((n, note)) = &v.omitted {
        let s = if note.is_empty() {
            format!("{n} more in All items · the suggested set may exceed five minutes")
        } else {
            format!("{n} more in All items · {note}")
        };
        g.put_str(r.x + 1, y, &s, t.dim(), r.w.saturating_sub(2));
        y += 1;
    }
    for c in &v.coverage {
        g.put_str(
            r.x + 1,
            y,
            &format!("⚠ {c}"),
            t.s(t.yellow),
            r.w.saturating_sub(2),
        );
        y += 1;
    }
    if let Some(n) = &st.notice {
        g.put_str(r.x + 1, y, n, t.s(t.yellow), r.w.saturating_sub(2));
        y += 1;
    }
    y += 1;
    let list_w = (r.w * 2 / 5).clamp(24.min(r.w), 60);
    let bottom = r.y + r.h.saturating_sub(1);
    let list_top = y;
    if v.items.is_empty() {
        g.put_str(r.x + 2, y, "Nothing needs you", t.dim(), list_w);
    }
    let multi = app.machines.len() > 1;
    let rows = bottom.saturating_sub(list_top) as usize;
    let skip = sel_idx.saturating_sub(rows.saturating_sub(2));
    let mut shown_footer = false;
    for (i, it) in v.items.iter().enumerate().skip(skip) {
        if y >= bottom {
            break;
        }
        if it.class == 5 && !shown_footer {
            shown_footer = true;
            g.put_str(r.x + 1, y, "─ Finished turns ─", t.dim(), list_w);
            y += 1;
            if y >= bottom {
                break;
            }
        }
        let selected = i == sel_idx;
        let base = if it.stale {
            t.dim()
        } else if selected {
            t.sel(t.fg)
        } else {
            t.text()
        };
        if selected {
            g.fill(
                SRect {
                    x: r.x,
                    y,
                    w: list_w,
                    h: 1,
                },
                base,
            );
        }
        let fresh = if st.fresh.contains(&it.key) {
            "● "
        } else {
            ""
        };
        let mlabel = if multi {
            format!("[{}] ", app.machines[it.key.machine].label)
        } else {
            String::new()
        };
        let urgent = if st.five_minute && it.urgent {
            "Urgent — may take longer · "
        } else {
            ""
        };
        let glyph_color = match it.class {
            1 | 2 => t.red,
            3 => t.yellow,
            4 => t.accent,
            _ => t.green,
        };
        let mut x = r.x + 1;
        x += g.put_str(x, y, fresh, Style { ..t.bold(t.red) }, 2);
        x += g.put_str(
            x,
            y,
            &format!("{} ", class_glyph(it)),
            Style {
                bg: base.bg,
                ..t.bold(glyph_color)
            },
            2,
        );
        let line = format!(
            "{urgent}{mlabel}{}{} · {}{}{}",
            it.title,
            batch_suffix(it),
            if it.explanation.is_empty() {
                fmt_age(it.age_ms)
            } else {
                it.explanation.clone()
            },
            blocks_suffix(it),
            crate::gateway::row_suffix(app, it.key.machine, it.interaction.as_deref())
        );
        g.put_str(x, y, &line, base, (r.x + list_w).saturating_sub(x));
        y += 1;
    }
    if v.snoozed > 0 && y < bottom {
        g.put_str(
            r.x + 1,
            y,
            &format!("{} snoozed", v.snoozed),
            t.dim(),
            list_w,
        );
        y += 1;
    }
    // Also working (15 §8.1): routine busy agents, never ranked, never actionable here.
    if !v.also_working.is_empty() && y + 1 < bottom {
        g.put_str(r.x + 1, y, "─ Also working ─", t.dim(), list_w);
        y += 1;
        for w in &v.also_working {
            if y >= bottom {
                break;
            }
            g.put_str(
                r.x + 2,
                y,
                &working_line(app, w),
                t.dim(),
                list_w.saturating_sub(1),
            );
            y += 1;
        }
    }
    // Separator + detail.
    for yy in list_top.saturating_sub(1)..bottom {
        g.put_str(r.x + list_w + 1, yy, "│", t.border(false), 1);
    }
    let dx = r.x + list_w + 3;
    let dw = (r.x + r.w).saturating_sub(dx + 1);
    let mut lines: Vec<(String, Style)> = Vec::new();
    if let Some(it) = v.items.get(sel_idx) {
        detail_lines(app, it, &mut lines);
    }
    for (yy, (s, stl)) in (list_top.saturating_sub(1)..bottom).zip(lines) {
        g.put_str(dx, yy, &s, stl, dw);
    }
    // Sub-chooser overlay at the bottom of the detail column.
    match &st.sub {
        Sub::Snooze(ch) => {
            let mut sl: Vec<(String, Style)> = vec![("Snooze until…".into(), t.bold(t.fg))];
            for (i, c) in SNOOZE_CHOICES.iter().enumerate() {
                let s = format!("{} {}. {c}", if i == ch.sel { ">" } else { " " }, i + 1);
                sl.push((
                    s,
                    if i == ch.sel {
                        t.sel(t.accent)
                    } else {
                        t.text()
                    },
                ));
            }
            if let Some(c) = &ch.custom {
                sl.push((format!("time (45m · 2h · HH:MM): {c}"), t.bold(t.fg)));
            }
            if let Some(e) = &ch.error {
                sl.push((e.clone(), t.s(t.red)));
            }
            sl.push((
                "Never resolves the item or tells the agent anything; a deadline or new risk wakes it".into(),
                t.dim(),
            ));
            let start = bottom.saturating_sub(sl.len() as u16 + 1);
            for (i, (s, stl)) in sl.into_iter().enumerate() {
                let yy = start + i as u16;
                g.fill(
                    SRect {
                        x: dx,
                        y: yy,
                        w: dw,
                        h: 1,
                    },
                    t.text(),
                );
                g.put_str(dx, yy, &s, stl, dw);
            }
        }
        Sub::Effort { sel } => {
            let mut sl: Vec<(String, Style)> = vec![("Effort for this task".into(), t.bold(t.fg))];
            for (i, (_, l)) in EFFORTS.iter().enumerate() {
                sl.push((
                    format!("{} {l}", if i == *sel { ">" } else { " " }),
                    if i == *sel { t.sel(t.accent) } else { t.text() },
                ));
            }
            let start = bottom.saturating_sub(sl.len() as u16 + 1);
            for (i, (s, stl)) in sl.into_iter().enumerate() {
                let yy = start + i as u16;
                g.fill(
                    SRect {
                        x: dx,
                        y: yy,
                        w: dw,
                        h: 1,
                    },
                    t.text(),
                );
                g.put_str(dx, yy, &s, stl, dw);
            }
        }
        Sub::None => {}
    }
    let keys = "j/k move · enter open · y/n/1-9 answer · A batch · o open pane · s snooze · e effort · f 5-minute view · esc close";
    g.put_str(r.x + 1, bottom, keys, t.dim(), r.w.saturating_sub(2));
}

/// One **Also working** footer row.
pub fn working_line(app: &App, w: &Working) -> String {
    let m = if app.machines.len() > 1 {
        format!("[{}] ", app.machines[w.machine].label)
    } else {
        String::new()
    };
    match &w.task {
        Some(t) => format!(
            "{m}{} · working {} · {t}",
            w.name,
            fmt_age(w.working_for_ms)
        ),
        None => format!("{m}{} · working {}", w.name, fmt_age(w.working_for_ms)),
    }
}

/// " ⧉N" for an item in a server-side batch of N equivalent approvals.
pub fn batch_suffix(it: &Item) -> String {
    match &it.batch {
        Some((_, n)) if *n > 1 => format!(" ⧉{n}"),
        _ => String::new(),
    }
}

/// The deadline line of the detail view (15 §8.1): the actual deadline, honestly sourced.
pub fn deadline_text(it: &Item, now: i64) -> Option<String> {
    let d = it.deadline_ms?;
    let left = fmt_age((d - now).max(0));
    Some(match it.deadline_source.as_deref() {
        Some("gate") => format!(
            "Answer here within {left}; after that the question shows in the agent's own pane"
        ),
        _ => format!("The agent's request expires in {left} (its own deadline)"),
    })
}

/// " · blocks N linked tasks" unless the server's explanation already says it (15 §8.1).
pub fn blocks_suffix(it: &Item) -> String {
    if it.blocks_tasks == 0 || it.explanation.contains("blocks ") {
        String::new()
    } else {
        format!(" · {}", crate::tasks_t4::blocks_text(it.blocks_tasks))
    }
}

fn detail_lines(app: &App, it: &Item, out: &mut Vec<(String, Style)>) {
    let t = app.theme;
    let mi = it.key.machine;
    out.push((it.title.clone(), t.bold(t.fg)));
    if !it.subtitle.is_empty() {
        out.push((it.subtitle.clone(), t.dim()));
    }
    let mut facts = Vec::new();
    if !it.explanation.is_empty() {
        facts.push(it.explanation.clone());
    }
    if let Some(r) = &it.risk {
        facts.push(format!("risk: {r}"));
    }
    if let Some(e) = &it.effort {
        facts.push(format!(
            "effort: {}",
            EFFORTS
                .iter()
                .find(|(v, _)| v == e)
                .map(|(_, l)| *l)
                .unwrap_or(e)
        ));
    }
    if it.effort.as_deref().is_none_or(|e| e == "unknown")
        && let Some((e, src)) = &it.effort_estimate
    {
        facts.push(format!(
            "estimate: {} ({src} — not set)",
            crate::tasks_t4::effort_label(e)
        ));
    }
    if it.blocks_tasks > 0 && !it.explanation.contains("blocks ") {
        facts.push(crate::tasks_t4::blocks_text(it.blocks_tasks));
    }
    if app.machines.len() > 1 {
        facts.push(format!("on {}", app.machines[mi].label));
    }
    if !facts.is_empty() {
        out.push((facts.join(" · "), t.text()));
    }
    if it.urgent {
        out.push(("Urgent — may take longer".into(), t.bold(t.red)));
    }
    if let Some(w) = &it.woke_from_snooze {
        out.push((format!("Woke from snooze: {w}"), t.s(t.yellow)));
    }
    if let Some(d) = deadline_text(it, now_ms()) {
        out.push((d, t.s(if it.class <= 2 { t.red } else { t.fg })));
    }
    if let Some((_, n)) = it.batch.as_ref().filter(|(_, n)| *n > 1) {
        out.push((
            format!(
                "Equivalent to {} other waiting approval(s) · [A] batch view (each is answered and delivered on its own)",
                n - 1
            ),
            t.dim(),
        ));
    }
    if it.stale {
        out.push((
            "Machine offline — last observed state; actions disabled".into(),
            t.s(t.red),
        ));
    }
    out.push((String::new(), t.text()));
    let m = &app.machines[mi];
    if let Some(int) = &it.interaction
        && let Some(x) = m.model.interactions.iter().find(|x| &x.id == int)
    {
        crate::popups::card_lines(app, mi, x, 0, out);
        let keys = if x.status != InteractionStatus::Open {
            "resolved — no longer answerable".to_string()
        } else if !x.answerable {
            "answer in the pane · [o] open pane".to_string()
        } else {
            match x.kind {
                InteractionKind::Approval => {
                    "[y] allow once  [n] deny  [enter] full card (session, message)  [o] open pane"
                        .into()
                }
                InteractionKind::PlanReview => {
                    "[y] approve  [n] reject  [enter] full card  [o] open pane".into()
                }
                InteractionKind::Question => "[1-9] pick  [enter] full card  [o] open pane".into(),
                InteractionKind::Notice => "[o] open pane".into(),
                InteractionKind::Picker => "[1-9] pick  [enter] full card  [o] open pane".into(),
            }
        };
        out.push((String::new(), t.text()));
        out.push((keys, t.dim()));
        if let Some(task) = it
            .task
            .as_ref()
            .and_then(|tid| m.model.tasks.iter().find(|x| &x.id == tid))
        {
            out.push((format!("Task: {} (#{})", task.title, task.handle), t.dim()));
        }
        return;
    }
    if let Some(tid) = &it.task {
        if let Some(task) = m.model.tasks.iter().find(|x| &x.id == tid) {
            out.push((format!("Task #{} · {}", task.handle, task.title), t.text()));
            let label = task
                .review_label
                .as_deref()
                .map(crate::tasks::label_text)
                .unwrap_or("Tracked");
            out.push((
                format!(
                    "{} · {} · {}",
                    label,
                    match task.ownership {
                        TaskOwnership::Attached => "attached",
                        TaskOwnership::Owned => "owned",
                    },
                    task.status
                ),
                t.dim(),
            ));
        }
        out.push((String::new(), t.text()));
        out.push(("[enter] Open task  [e] effort  [s] snooze".into(), t.dim()));
        return;
    }
    if it.pane.is_some() {
        out.push(("[enter] focus pane".into(), t.dim()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_app;
    use vk_proto::input::Mods;
    use vk_proto::render::{ClientFrame, ServerFrame};

    fn item(kind: &str, id: &str, class: Value, age: i64, urgent: bool) -> Value {
        json!({"key": {"kind": kind, "id": id}, "class": class, "title": format!("T {id}"),
               "subtitle": "", "task": null, "run": null, "pane": null, "interaction": null,
               "explanation": format!("Waiting {}m", age / 60000), "age_ms": age, "risk": null,
               "effort": null, "snoozed_until_ms": null, "woke_from_snooze": null, "urgent": urgent})
    }

    fn commands(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientFrame>) -> Vec<(u64, Value)> {
        let mut v = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if let ClientFrame::Command { req, json } = f {
                v.push((req, serde_json::from_str(&json).unwrap()));
            }
        }
        v
    }

    fn reply(app: &mut App, mi: usize, req: u64, result: Value) {
        let json = json!({"jsonrpc": "2.0", "id": req, "result": result}).to_string();
        app.on_frame(mi, ServerFrame::CommandResult { req, json });
    }

    fn key(c: Key) -> KeyEvent {
        KeyEvent::new(c, Mods::empty())
    }

    #[test]
    fn merges_machines_by_precedence_and_keeps_server_order() {
        let a = vec![
            parse_item(0, &item("review", "a1", json!(4), 50_000, false)).unwrap(),
            parse_item(0, &item("review", "a2", json!(4), 900_000, false)).unwrap(),
        ];
        let b = vec![
            parse_item(1, &item("interaction", "b1", json!(3), 10_000, false)).unwrap(),
            parse_item(1, &item("send_unknown", "b2", json!(1), 1_000, true)).unwrap(),
            parse_item(
                1,
                &item("finished_turn", "b3", json!("finished_turns"), 5, false),
            )
            .unwrap(),
        ];
        let ids: Vec<String> = merge(vec![a, b]).into_iter().map(|i| i.key.id).collect();
        // b2 is class 1 but behind b1 in its own server order: server order wins within a
        // machine; a1 stays ahead of a2 although a2 is older.
        assert_eq!(ids, vec!["b1", "b2", "a1", "a2", "b3"]);
    }

    #[test]
    fn inbox_merges_and_keeps_selection_when_items_reorder() {
        let (mut app, mut rxs) = test_app(2);
        open(&mut app);
        let c0 = commands(&mut rxs[0]);
        let c1 = commands(&mut rxs[1]);
        assert_eq!(c0[0].1["method"], "attention.list");
        assert_eq!(c1[0].1["method"], "attention.list");
        reply(
            &mut app,
            0,
            c0[0].0,
            json!({"items": [item("review", "x", json!(4), 60_000, false), item("review", "y", json!(4), 30_000, false)],
                   "coverage": {"complete": true, "notes": []}, "five_minute": null}),
        );
        reply(
            &mut app,
            1,
            c1[0].0,
            json!({"items": [item("interaction", "z", json!(3), 1_000, false)],
                   "coverage": {"complete": false, "notes": ["events truncated; snapshot pending"]}, "five_minute": null}),
        );
        let v = view(&app);
        let ids: Vec<&str> = v.items.iter().map(|i| i.key.id.as_str()).collect();
        assert_eq!(ids, vec!["z", "x", "y"]);
        assert!(
            v.coverage
                .iter()
                .any(|c| c.contains("m1: events truncated"))
        );
        // Select y, then the server reorders and a new urgent item arrives on top.
        key_down(&mut app, 2);
        assert_eq!(app.inbox.selected.as_ref().unwrap().id, "y");
        app.inbox.last_fetch = None;
        refresh(&mut app);
        let c0 = commands(&mut rxs[0]);
        let c1 = commands(&mut rxs[1]);
        reply(
            &mut app,
            0,
            c0[0].0,
            json!({"items": [item("review", "y", json!(4), 61_000, false), item("review", "x", json!(4), 90_000, false)],
                   "coverage": {"complete": true, "notes": []}}),
        );
        reply(
            &mut app,
            1,
            c1[0].0,
            json!({"items": [item("send_unknown", "u", json!(1), 10, true), item("interaction", "z", json!(3), 2_000, false)],
                   "coverage": {"complete": true, "notes": []}}),
        );
        assert_eq!(app.inbox.selected.as_ref().unwrap().id, "y");
        let v = view(&app);
        assert_eq!(v.items[app.inbox.sel_idx].key.id, "y");
        // The new urgent item is flagged rather than stealing the selection.
        assert!(app.inbox.fresh.iter().any(|k| k.id == "u"));
        // The selected item resolves: selection moves to its neighbour with a notice.
        reply_all_with(
            &mut app,
            &mut rxs,
            vec![
                json!({"items": [item("review", "x", json!(4), 90_000, false)], "coverage": {"complete": true, "notes": []}}),
                json!({"items": [item("interaction", "z", json!(3), 2_000, false)], "coverage": {"complete": true, "notes": []}}),
            ],
        );
        assert!(app.inbox.notice.is_some());
        assert!(app.inbox.selected.is_some());
    }

    fn key_down(app: &mut App, n: usize) {
        for _ in 0..n {
            app.on_key(key(Key::Char('j')));
        }
    }

    fn reply_all_with(
        app: &mut App,
        rxs: &mut [tokio::sync::mpsc::UnboundedReceiver<ClientFrame>],
        results: Vec<Value>,
    ) {
        app.inbox.last_fetch = None;
        refresh(app);
        for (mi, r) in results.into_iter().enumerate() {
            let c = commands(&mut rxs[mi]);
            reply(app, mi, c[0].0, r);
        }
    }

    #[test]
    fn falls_back_to_m1_when_method_is_missing() {
        let (mut app, mut rxs) = test_app(1);
        app.machines[0]
            .model
            .interactions
            .push(crate::app::test_interaction(
                "i1",
                "p9",
                "Allow rm?",
                now_ms() - 120_000,
            ));
        open(&mut app);
        let c = commands(&mut rxs[0]);
        let json = json!({"jsonrpc":"2.0","id":c[0].0,"error":{"code":-32601,"message":"method not found","data":{"kind":"method_not_found"}}}).to_string();
        app.on_frame(0, ServerFrame::CommandResult { req: c[0].0, json });
        assert!(matches!(app.inbox.per_machine[0], Source::Unsupported));
        let v = view(&app);
        assert_eq!(v.items.len(), 1);
        assert_eq!(v.items[0].key.id, "i1");
        assert!(v.items[0].fallback);
        // No toast for the expected degradation; the coverage line says it instead.
        assert!(app.toasts.is_empty());
        // prefix+a with no ranking anywhere keeps the M1 behaviour (card for the oldest).
        app.mode = Mode::Normal;
        next_attention(&mut app, false);
        assert!(
            matches!(&app.mode, Mode::Popup(Popup::Card { interaction, .. }) if interaction == "i1")
        );
    }

    #[test]
    fn five_minute_view_counts_omitted_and_keeps_urgent() {
        let (mut app, mut rxs) = test_app(1);
        app.size = (140, 30);
        open(&mut app);
        commands(&mut rxs[0]);
        app.on_key(key(Key::Char('f')));
        let c = commands(&mut rxs[0]);
        assert_eq!(c[0].1["params"]["budget_ms"], 300000);
        reply(
            &mut app,
            0,
            c[0].0,
            json!({"items": [item("send_unknown", "u", json!(1), 10, true),
                              item("review", "a", json!(4), 600_000, false),
                              item("review", "b", json!(4), 60_000, false)],
                   "coverage": {"complete": true, "notes": []},
                   "five_minute": {"keys": [{"kind": "review", "id": "a"}], "omitted_count": 1,
                                   "note": "may exceed five minutes"}}),
        );
        let v = view(&app);
        let ids: Vec<&str> = v.items.iter().map(|i| i.key.id.as_str()).collect();
        assert_eq!(ids, vec!["u", "a"]);
        assert_eq!(v.omitted.as_ref().unwrap().0, 1);
        let mut g = Grid::new(140, 30);
        draw(&app, &mut g);
        let text = crate::tasks::grid_text(&g);
        assert!(text.contains("[5-minute view]"), "{text}");
        assert!(text.contains("1 more in All items"), "{text}");
        assert!(text.contains("Urgent — may take longer"), "{text}");
    }

    #[test]
    fn snooze_chooser_states() {
        let now = 1_700_000_000_000;
        let mut ch = SnoozeChooser::default();
        assert_eq!(
            ch.key(&key(Key::Named(NamedKey::Enter)), now),
            SnoozeOutcome::Until(now + 15 * 60_000)
        );
        ch.key(&key(Key::Char('j')), now);
        assert_eq!(
            ch.key(&key(Key::Named(NamedKey::Enter)), now),
            SnoozeOutcome::Until(now + 3_600_000)
        );
        // Tomorrow 9:00 is in the future and at most ~33h away.
        match ch.key(&key(Key::Char('3')), now) {
            SnoozeOutcome::Until(t) => assert!(t > now && t - now <= 33 * 3_600_000),
            o => panic!("{o:?}"),
        }
        // Custom: bad input keeps the chooser open with an error, good input resolves.
        assert_eq!(ch.key(&key(Key::Char('4')), now), SnoozeOutcome::Stay);
        assert_eq!(ch.custom.as_deref(), Some(""));
        for c in "zz".chars() {
            ch.key(&key(Key::Char(c)), now);
        }
        assert_eq!(
            ch.key(&key(Key::Named(NamedKey::Enter)), now),
            SnoozeOutcome::Stay
        );
        assert!(ch.error.is_some());
        ch.key(&key(Key::Named(NamedKey::Backspace)), now);
        ch.key(&key(Key::Named(NamedKey::Backspace)), now);
        for c in "45m".chars() {
            ch.key(&key(Key::Char(c)), now);
        }
        assert_eq!(
            ch.key(&key(Key::Named(NamedKey::Enter)), now),
            SnoozeOutcome::Until(now + 45 * 60_000)
        );
        // Esc leaves custom entry first, then cancels.
        ch.key(&key(Key::Named(NamedKey::Escape)), now);
        assert_eq!(ch.custom, None);
        assert_eq!(
            ch.key(&key(Key::Named(NamedKey::Escape)), now),
            SnoozeOutcome::Cancel
        );
        assert!(parse_custom("17:30", now).is_some_and(|t| t > now));
        assert_eq!(parse_custom("25:00", now), None);
    }

    #[test]
    fn snooze_sends_attention_update_with_server_key() {
        let (mut app, mut rxs) = test_app(1);
        open(&mut app);
        let c = commands(&mut rxs[0]);
        reply(
            &mut app,
            0,
            c[0].0,
            json!({"items": [item("review", "a", json!(4), 600_000, false)], "coverage": {"complete": true, "notes": []}}),
        );
        app.on_key(key(Key::Char('s')));
        assert!(matches!(app.inbox.sub, Sub::Snooze(_)));
        app.on_key(key(Key::Char('2')));
        let c = commands(&mut rxs[0]);
        let upd = c
            .iter()
            .find(|(_, v)| v["method"] == "attention.update")
            .unwrap();
        assert_eq!(upd.1["params"]["key"], json!({"kind": "review", "id": "a"}));
        assert!(upd.1["params"]["snooze_until_ms"].as_i64().unwrap() > now_ms());
        assert!(matches!(app.mode, Mode::Popup(Popup::Inbox)));
    }

    #[test]
    fn next_attention_follows_the_ranking_and_skips_snoozed() {
        let (mut app, mut rxs) = test_app(1);
        app.machines[0]
            .model
            .interactions
            .push(crate::app::test_interaction(
                "i1",
                "p2",
                "Allow rm?",
                now_ms() - 600_000,
            ));
        next_attention(&mut app, false);
        let c = commands(&mut rxs[0]);
        assert_eq!(c[0].1["method"], "attention.list");
        // Nothing opens until the ranking arrives.
        assert!(matches!(app.mode, Mode::Normal));
        let mut snoozed = item("review", "t0", json!(3), 900_000, false);
        snoozed["task"] = json!("t0");
        snoozed["snoozed_until_ms"] = json!(now_ms() + 3_600_000);
        let mut review = item("review", "t1:abc", json!(4), 60_000, false);
        review["task"] = json!("t1");
        let mut int = item("interaction", "i1", json!(4), 600_000, false);
        int["interaction"] = json!("i1");
        int["pane"] = json!("p2");
        // The server ranks the review above the (older) interaction: follow it.
        reply(
            &mut app,
            0,
            c[0].0,
            json!({"items": [snoozed, review, int], "coverage": {"complete": true, "notes": []}}),
        );
        assert!(matches!(app.mode, Mode::Popup(Popup::Task)));
        assert_eq!(app.task_view.as_ref().unwrap().task, "t1");
        // No focus change happened.
        assert_eq!(app.focused_pane(), None);
    }

    #[test]
    fn inbox_card_answer_returns_to_inbox() {
        let (mut app, mut rxs) = test_app(1);
        app.machines[0]
            .model
            .interactions
            .push(crate::app::test_interaction(
                "i1",
                "p2",
                "Allow rm?",
                now_ms() - 60_000,
            ));
        open(&mut app);
        let c = commands(&mut rxs[0]);
        let mut int = item("interaction", "i1", json!(3), 60_000, false);
        int["interaction"] = json!("i1");
        reply(
            &mut app,
            0,
            c[0].0,
            json!({"items": [int], "coverage": {"complete": true, "notes": []}}),
        );
        app.on_key(key(Key::Named(NamedKey::Enter)));
        assert!(matches!(app.mode, Mode::Popup(Popup::Card { .. })));
        app.on_key(key(Key::Char('y')));
        let c = commands(&mut rxs[0]);
        assert!(c.iter().any(|(_, v)| v["method"] == "interaction.answer"));
        assert!(matches!(app.mode, Mode::Popup(Popup::Inbox)));
        // Quick answer from the inbox itself uses the same path.
        app.on_key(key(Key::Char('n')));
        let c = commands(&mut rxs[0]);
        let a = c
            .iter()
            .find(|(_, v)| v["method"] == "interaction.answer")
            .unwrap();
        assert_eq!(a.1["params"]["decision"], "deny");
        app.on_key(key(Key::Named(NamedKey::Escape)));
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn sidebar_shows_review_marker_next_to_done() {
        let (mut app, _rxs) = test_app(1);
        app.machines[0]
            .model
            .runs
            .push(crate::app::test_run("r1", "p1", "claude"));
        app.machines[0].model.tasks.push(Task {
            id: "t1".into(),
            handle: "1".into(),
            title: "x".into(),
            status: "active".into(),
            intent_revision: Some(1),
            review_label: Some("review_available".into()),
            ..Default::default()
        });
        app.task_runs.insert((0, "r1".into()), "t1".into());
        let row = crate::draw::agent_row_text(&app, 0, "r1");
        assert!(row.contains("✓ "), "{row}");
        assert!(row.contains("◆review"), "{row}");
        assert!(row.contains("done"), "{row}");
    }

    #[test]
    fn inbox_draws_items_details_and_coverage() {
        let (mut app, mut rxs) = test_app(2);
        app.size = (150, 32);
        app.machines[1].tx = None;
        app.machines[1].status = "offline".into();
        app.machines[0].model.tasks.push(Task {
            id: "t1".into(),
            handle: "3".into(),
            title: "Fix login redirect".into(),
            status: "active".into(),
            ownership: TaskOwnership::Attached,
            review_label: Some("review_available".into()),
            ..Default::default()
        });
        open(&mut app);
        let c = commands(&mut rxs[0]);
        let mut it = item("review", "t1:abc", json!(4), 120_000, false);
        it["task"] = json!("t1");
        it["title"] = json!("Fix login redirect");
        it["explanation"] = json!("Review available · one required check missing");
        reply(
            &mut app,
            0,
            c[0].0,
            json!({"items": [it], "coverage": {"complete": true, "notes": []}}),
        );
        let mut g = Grid::new(150, 32);
        draw(&app, &mut g);
        let text = crate::tasks::grid_text(&g);
        assert!(text.contains("Needs you"), "{text}");
        assert!(text.contains("Fix login redirect"), "{text}");
        assert!(text.contains("one required check missing"), "{text}");
        assert!(text.contains("[enter] Open task"), "{text}");
        assert!(text.contains("m1 offline"), "{text}");
    }

    /// 15 §8.3 / Codex G02 #19: the server woke a snoozed item after a material change but kept
    /// the old (future) deadline in the metadata; the item must be visible again, even though the
    /// previous listing said it was snoozed.
    #[test]
    fn snooze_wake_from_server_is_visible_despite_old_deadline() {
        let (mut app, mut rxs) = test_app(1);
        open(&mut app);
        let c = commands(&mut rxs[0]);
        let until = now_ms() + 3_600_000;
        let mut it = item("review", "t1:abc", json!(4), 120_000, false);
        it["task"] = json!("t1");
        it["title"] = json!("Fix login redirect");
        it["snoozed_until_ms"] = json!(until);
        let mut other = item("review", "t2:def", json!(4), 60_000, false);
        other["title"] = json!("Still snoozed");
        other["snoozed_until_ms"] = json!(until);
        reply(
            &mut app,
            0,
            c[0].0,
            json!({"items": [it.clone(), other.clone()], "coverage": {"complete": true, "notes": []}}),
        );
        let v = view(&app);
        assert!(v.items.is_empty());
        assert_eq!(v.snoozed, 2);
        // Evidence changed: the server wakes t1 (deadline left in place) and leaves t2 snoozed.
        it["woke_from_snooze"] = json!("New check failure on this subject");
        refresh(&mut app);
        let c = commands(&mut rxs[0]);
        reply(
            &mut app,
            0,
            c[0].0,
            json!({"items": [it, other], "coverage": {"complete": true, "notes": []}}),
        );
        let v = view(&app);
        assert_eq!(v.items.len(), 1);
        assert_eq!(v.items[0].title, "Fix login redirect");
        assert_eq!(v.snoozed, 1);
        let mut g = Grid::new(150, 32);
        draw(&app, &mut g);
        let text = crate::tasks::grid_text(&g);
        assert!(text.contains("Fix login redirect"), "{text}");
        assert!(
            text.contains("Woke from snooze: New check failure"),
            "{text}"
        );
        assert!(!text.contains("Still snoozed"), "{text}");
        // A null deadline is never hidden either.
        let mut cleared = item("review", "t3:x", json!(4), 1_000, false);
        cleared["snoozed_until_ms"] = Value::Null;
        assert!(!parse_item(0, &cleared).unwrap().snoozed(now_ms()));
    }

    /// Lane 2C (15 §8.1, §8.3): the actual deadline with its source, the server's batch
    /// membership with `A` opening the batch view, and the Also working footer.
    #[test]
    fn deadline_batch_and_also_working_are_shown() {
        let (mut app, mut rxs) = test_app(1);
        app.size = (160, 36);
        open(&mut app);
        let c = commands(&mut rxs[0]);
        let mut a = item("interaction", "i1", json!(2), 30_000, true);
        a["interaction"] = json!("i1");
        a["title"] = json!("Run cargo test?");
        a["explanation"] = json!("Deadline in 40s · waiting 30s · blocks this run");
        a["deadline_ms"] = json!(now_ms() + 40_000);
        a["deadline_source"] = json!("native");
        a["batch"] = json!({"id": "b_1", "size": 2});
        let mut g2 = item("interaction", "i2", json!(3), 10_000, false);
        g2["deadline_ms"] = json!(now_ms() + 1_800_000);
        g2["deadline_source"] = json!("gate");
        reply(
            &mut app,
            0,
            c[0].0,
            json!({"items": [a, g2], "coverage": {"complete": true, "notes": []},
                   "also_working": [{"run": "r9", "pane": "p9", "name": "codex", "harness": "codex",
                                     "task": {"id": "t9", "handle": "9", "title": "Migrate sessions"},
                                     "since_ms": 0, "working_for_ms": 125_000}]}),
        );
        let v = view(&app);
        assert_eq!(v.items[0].batch, Some(("b_1".to_string(), 2)));
        assert_eq!(v.items[0].deadline_source.as_deref(), Some("native"));
        assert_eq!(v.also_working.len(), 1);
        let mut g = Grid::new(160, 36);
        draw(&app, &mut g);
        let text = crate::tasks::grid_text(&g);
        assert!(text.contains("⧉2"), "{text}");
        assert!(text.contains("expires in"), "{text}");
        assert!(text.contains("[A] batch view"), "{text}");
        assert!(text.contains("Also working"), "{text}");
        assert!(text.contains("codex · working 2m"), "{text}");
        assert!(text.contains("Migrate sessions"), "{text}");
        // The gate deadline reads differently (the pane dialog follows).
        let gate = parse_item(
            0,
            &json!({"key": {"kind": "interaction", "id": "x"}, "class": 3,
            "deadline_ms": now_ms() + 60_000, "deadline_source": "gate"}),
        )
        .unwrap();
        assert!(
            deadline_text(&gate, now_ms())
                .unwrap()
                .contains("agent's own pane")
        );
        // ui.inbox.also_working = false hides the footer.
        app.config.ui.inbox.also_working = false;
        assert!(view(&app).also_working.is_empty());
    }

    #[test]
    fn also_working_falls_back_to_the_session_model() {
        let (mut app, _rxs) = test_app(1);
        let mut r = crate::app::test_run("r1", "p1", "claude");
        r.execution.value = Execution::Working;
        app.machines[0].model.runs.push(r);
        let w = working_from_model(&app, 0);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].run, "r1");
        assert!(working_line(&app, &w[0]).contains("working"));
    }
}
