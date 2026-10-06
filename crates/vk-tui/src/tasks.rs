//! Task surfaces (15 §2.2–§2.6): **Track this work**, the task detail view, intent edits with
//! optional clarification, review acceptance with explicit exceptions and explicit check runs.
//!
//! Everything here is opened by the user and draws only in the pane area while open. Saving never
//! sends; sending is a separate explicit action whose refusal writes zero bytes and offers
//! **Open pane to send**. Mutations go through [`App::mutate`], which persists the idempotency key
//! before dispatch (15 §10.3).

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::draw::truncate;
use crate::screen::{Grid, Rect as SRect};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use vk_proto::input::{Key, KeyEvent, NamedKey};
use vk_proto::model::*;
use vk_proto::render::Style;

/// `vk_review::intent::StopAt` values with their labels; the form starts at **Not specified**.
pub const STOP_AT: [(&str, &str); 8] = [
    ("unspecified", "Not specified"),
    ("implementation", "Implementation"),
    ("draft_pr", "Draft PR"),
    ("reviewed_pr", "Reviewed PR"),
    ("merge", "Merge"),
    ("verified_deployment", "Verified deployment"),
    ("custom", "Custom"),
    (
        "no_separate_outcome",
        "No separate delivery outcome to verify",
    ),
];

pub fn stop_label(v: &str) -> &'static str {
    STOP_AT
        .iter()
        .find(|(k, _)| *k == v)
        .map(|(_, l)| *l)
        .unwrap_or("Not specified")
}

/// Review label text (15 §7).
pub fn label_text(l: &str) -> &'static str {
    match l {
        "turn_finished" => "Turn finished",
        "needs_task_details" => "Needs task details",
        "changes_to_inspect" => "Changes to inspect",
        "review_available" => "Review available",
        "ready_for_review" => "Ready for your review",
        "reviewed" => "Reviewed",
        "reviewed_with_exceptions" => "Reviewed with exceptions",
        "review_outdated" => "Review outdated",
        "in_progress" => "In progress",
        "finished_without_review" => "Finished without review",
        _ => "Tracked",
    }
}

/// Small sidebar marker for a tracked task's review label: (text, tone) where tone is
/// 0 muted, 1 accent, 2 attention, 3 good.
pub fn sidebar_marker(l: Option<&str>) -> (&'static str, u8) {
    match l {
        Some("needs_task_details") => ("◇details", 2),
        Some("changes_to_inspect") => ("◆changes", 1),
        Some("review_available") => ("◆review", 1),
        Some("ready_for_review") => ("◆ready", 1),
        Some("reviewed") => ("◆reviewed", 3),
        Some("reviewed_with_exceptions") => ("◆reviewed*", 3),
        Some("review_outdated") => ("◆outdated", 2),
        Some("finished_without_review") => ("◆unreviewed", 0),
        _ => ("◆", 0),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) fn st<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

pub(crate) fn arr<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get(k)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Text input: printable characters append, Backspace deletes, ctrl+u clears.
pub(crate) fn edit(buf: &mut String, ev: &KeyEvent) -> bool {
    match ev.key {
        Key::Named(NamedKey::Backspace) => {
            buf.pop();
            true
        }
        Key::Char('u') if ev.mods.ctrl() => {
            buf.clear();
            true
        }
        Key::Char(c) if !ev.mods.ctrl() && !ev.mods.alt() => {
            buf.push(c);
            true
        }
        _ => false,
    }
}

fn is_tab(ev: &KeyEvent) -> Option<bool> {
    match ev.key {
        Key::Named(NamedKey::Tab) if ev.mods.shift() => Some(false),
        Key::Named(NamedKey::Tab) => Some(true),
        _ => None,
    }
}

fn ctrl(ev: &KeyEvent, c: char) -> bool {
    ev.mods.ctrl() && ev.key == Key::Char(c)
}

// ---- replies ------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Reply {
    Sources {
        form: u64,
    },
    Track {
        form: u64,
    },
    Detail {
        view: u64,
    },
    Review {
        view: u64,
    },
    Checks {
        view: u64,
    },
    IntentSaved {
        view: u64,
        send: bool,
    },
    Prepared {
        view: u64,
    },
    Sent {
        view: u64,
    },
    MessageGet {
        view: u64,
    },
    Bound {
        view: u64,
    },
    Accepted {
        view: u64,
    },
    CheckRun {
        view: u64,
        /// The subject the run was submitted for.
        subject: String,
    },
    /// `task.detail` for the run → task map behind sidebar markers and peek.
    TaskRuns {
        task: String,
    },
    /// T4 surfaces (snapshot, reviewer, notes, dependencies, effort): `crate::tasks_t4`.
    T4(crate::tasks_t4::T4Reply),
    Lane2c(crate::tasks_2c::R),
}

// ---- Track this work ----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct SourceTurn {
    pub n: u32,
    pub prompt: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TrackPhase {
    Loading,
    /// Run identity not verified: no Track action.
    Unverified,
    Ready,
    Submitting,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrackField {
    Source,
    Title,
    Constraint(usize),
    Criterion(usize),
    AddCriterion,
    Stop,
    Objective,
    Submit,
}

/// A Track-form criterion or constraint. A typed one is a required human criterion (sent as a
/// plain string, as before); one filled from an assistant suggestion keeps its generated
/// semantics — optional, its evaluation kind and the source turns it cites (14, 15 §2.2).
#[derive(Debug, Clone, PartialEq)]
pub struct TrackItem {
    pub text: String,
    pub required: bool,
    pub evaluation: Option<String>,
    pub source_turns: Vec<u32>,
}

impl TrackItem {
    pub fn typed(text: &str) -> Self {
        TrackItem {
            text: text.into(),
            required: true,
            evaluation: None,
            source_turns: vec![],
        }
    }

    fn plain(&self) -> bool {
        self.required && self.evaluation.is_none() && self.source_turns.is_empty()
    }

    /// Wire form for `task.track`: a string when nothing but the text is set, else an object.
    fn criterion_param(&self) -> Value {
        if self.plain() {
            return json!(self.text.trim());
        }
        let mut v = json!({"text": self.text.trim(), "required": self.required});
        if let Some(e) = &self.evaluation {
            v["evaluation"] = json!(e);
        }
        if !self.source_turns.is_empty() {
            v["source_turns"] = json!(self.source_turns);
        }
        v
    }

    fn constraint_param(&self) -> Value {
        if self.source_turns.is_empty() {
            return json!(self.text.trim());
        }
        json!({"text": self.text.trim(), "source_turns": self.source_turns})
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackForm {
    pub id: u64,
    pub machine: usize,
    pub run: String,
    pub pane: String,
    /// Generated once when the form opens; reused by every retry of this form.
    pub idem: String,
    pub phase: TrackPhase,
    pub turns: Vec<SourceTurn>,
    pub sel_turn: usize,
    pub title: String,
    pub title_edited: bool,
    pub criteria: Vec<TrackItem>,
    /// Constraints (filled from a suggestion; editable and removable, never criteria).
    pub constraints: Vec<TrackItem>,
    pub stop: usize,
    pub objective: String,
    pub field: TrackField,
    pub error: Option<String>,
    /// Fields were filled from an assistant suggestion (14; still unsaved and editable).
    pub assisted: bool,
    /// `task.link.status` for an unverified run (lane 2C **Link run**, `crate::tasks_2c`).
    pub link: Option<Value>,
    pub link_sel: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FormOutcome {
    Stay,
    Cancel,
    Submit,
}

pub fn first_line(s: &str) -> String {
    let l = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    truncate(l, 80)
}

impl TrackForm {
    pub fn new(id: u64, machine: usize, run: &str, pane: &str, idem: String) -> Self {
        TrackForm {
            id,
            machine,
            run: run.into(),
            pane: pane.into(),
            idem,
            phase: TrackPhase::Loading,
            turns: Vec::new(),
            sel_turn: 0,
            title: String::new(),
            title_edited: false,
            criteria: Vec::new(),
            constraints: Vec::new(),
            stop: 0,
            objective: String::new(),
            field: TrackField::Source,
            error: None,
            assisted: false,
            link: None,
            link_sel: 0,
        }
    }

    /// Apply a `task.sources` result.
    pub fn load_sources(&mut self, v: &Value) {
        self.turns = arr(v, "turns")
            .iter()
            .filter_map(|t| {
                let prompt = st(t, "prompt").to_string();
                (!prompt.trim().is_empty()).then(|| SourceTurn {
                    n: t.get("n").and_then(Value::as_u64).unwrap_or(0) as u32,
                    prompt,
                    truncated: t
                        .get("prompt_truncated")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect();
        // Latest first; the latest is preselected.
        self.turns.sort_by_key(|t| std::cmp::Reverse(t.n));
        self.sel_turn = 0;
        if let Some(r) = v.get("run").and_then(Value::as_str) {
            self.run = r.into();
        }
        if v.get("identity_verified").and_then(Value::as_bool) == Some(false) {
            self.phase = TrackPhase::Unverified;
            return;
        }
        self.phase = TrackPhase::Ready;
        if !self.title_edited {
            self.title = self
                .turns
                .first()
                .map(|t| first_line(&t.prompt))
                .unwrap_or_default();
        }
        self.field = if self.turns.is_empty() {
            TrackField::Title
        } else {
            TrackField::Source
        };
    }

    fn fields(&self) -> Vec<TrackField> {
        let mut f = vec![TrackField::Source, TrackField::Title];
        f.extend((0..self.constraints.len()).map(TrackField::Constraint));
        f.extend((0..self.criteria.len()).map(TrackField::Criterion));
        f.extend([
            TrackField::AddCriterion,
            TrackField::Stop,
            TrackField::Objective,
            TrackField::Submit,
        ]);
        f
    }

    fn step(&mut self, fwd: bool) {
        let f = self.fields();
        let i = f.iter().position(|x| *x == self.field).unwrap_or(0);
        let j = if fwd {
            (i + 1).min(f.len() - 1)
        } else {
            i.saturating_sub(1)
        };
        self.field = f[j];
    }

    pub fn can_submit(&self) -> bool {
        self.phase == TrackPhase::Ready && !self.title.trim().is_empty()
    }

    pub fn key(&mut self, ev: &KeyEvent) -> FormOutcome {
        if ev.key == Key::Named(NamedKey::Escape) {
            return FormOutcome::Cancel;
        }
        if self.phase != TrackPhase::Ready {
            return FormOutcome::Stay;
        }
        if ctrl(ev, 's') {
            return if self.can_submit() {
                FormOutcome::Submit
            } else {
                FormOutcome::Stay
            };
        }
        if let Some(fwd) = is_tab(ev) {
            self.step(fwd);
            return FormOutcome::Stay;
        }
        match (self.field, ev.key) {
            (TrackField::Source, Key::Named(NamedKey::Down)) => {
                if self.sel_turn + 1 < self.turns.len() {
                    self.sel_turn += 1;
                    self.retitle();
                }
            }
            (TrackField::Source, Key::Named(NamedKey::Up)) => {
                if self.sel_turn > 0 {
                    self.sel_turn -= 1;
                    self.retitle();
                }
            }
            (TrackField::Source, Key::Named(NamedKey::Enter)) => self.step(true),
            (_, Key::Named(NamedKey::Down)) => self.step(true),
            (_, Key::Named(NamedKey::Up)) => self.step(false),
            (TrackField::Title, Key::Named(NamedKey::Enter))
            | (TrackField::Objective, Key::Named(NamedKey::Enter))
            | (TrackField::Stop, Key::Named(NamedKey::Enter)) => self.step(true),
            (TrackField::Title, _) => {
                if edit(&mut self.title, ev) {
                    self.title_edited = true;
                }
            }
            (TrackField::Objective, _) => {
                edit(&mut self.objective, ev);
            }
            (TrackField::Constraint(_), Key::Named(NamedKey::Enter)) => self.step(true),
            (TrackField::Constraint(i), _) if ctrl(ev, 'd') => self.remove_constraint(i),
            (TrackField::Constraint(i), Key::Named(NamedKey::Backspace))
                if self.constraints[i].text.is_empty() =>
            {
                self.remove_constraint(i)
            }
            (TrackField::Constraint(i), _) => {
                edit(&mut self.constraints[i].text, ev);
            }
            (TrackField::Criterion(i), Key::Named(NamedKey::Enter)) => {
                self.criteria.insert(i + 1, TrackItem::typed(""));
                self.field = TrackField::Criterion(i + 1);
            }
            (TrackField::Criterion(i), _) if ctrl(ev, 'r') => {
                self.criteria[i].required = !self.criteria[i].required
            }
            (TrackField::Criterion(i), _) if ctrl(ev, 'd') => self.remove_criterion(i),
            (TrackField::Criterion(i), Key::Named(NamedKey::Backspace))
                if self.criteria[i].text.is_empty() =>
            {
                self.remove_criterion(i)
            }
            (TrackField::Criterion(i), _) => {
                edit(&mut self.criteria[i].text, ev);
            }
            (TrackField::AddCriterion, Key::Named(NamedKey::Enter) | Key::Char(' ')) => {
                self.criteria.push(TrackItem::typed(""));
                self.field = TrackField::Criterion(self.criteria.len() - 1);
            }
            (TrackField::Stop, Key::Named(NamedKey::Right) | Key::Char(' ' | 'l')) => {
                self.stop = (self.stop + 1) % STOP_AT.len()
            }
            (TrackField::Stop, Key::Named(NamedKey::Left) | Key::Char('h')) => {
                self.stop = (self.stop + STOP_AT.len() - 1) % STOP_AT.len()
            }
            (TrackField::Submit, Key::Named(NamedKey::Enter)) if self.can_submit() => {
                return FormOutcome::Submit;
            }
            _ => {}
        }
        FormOutcome::Stay
    }

    fn remove_constraint(&mut self, i: usize) {
        if i < self.constraints.len() {
            self.constraints.remove(i);
        }
        self.field = if self.constraints.is_empty() {
            TrackField::Title
        } else {
            TrackField::Constraint(i.min(self.constraints.len() - 1))
        };
    }

    fn remove_criterion(&mut self, i: usize) {
        if i < self.criteria.len() {
            self.criteria.remove(i);
        }
        self.field = if i == 0 {
            if self.criteria.is_empty() {
                TrackField::AddCriterion
            } else {
                TrackField::Criterion(0)
            }
        } else {
            TrackField::Criterion(i - 1)
        };
    }

    fn retitle(&mut self) {
        if !self.title_edited
            && let Some(t) = self.turns.get(self.sel_turn)
        {
            self.title = first_line(&t.prompt);
        }
    }

    /// `task.track` params. No semantic extraction: the request stays verbatim on the server.
    pub fn params(&self) -> Value {
        let mut p = json!({
            "run": self.run,
            "title": self.title.trim(),
            "stop_at": STOP_AT[self.stop].0,
            "idempotency_key": self.idem,
        });
        if let Some(t) = self.turns.get(self.sel_turn) {
            p["turn"] = json!(t.n);
        }
        let crit: Vec<Value> = self
            .criteria
            .iter()
            .filter(|c| !c.text.trim().is_empty())
            .map(TrackItem::criterion_param)
            .collect();
        if !crit.is_empty() {
            p["criteria"] = json!(crit);
        }
        let cons: Vec<Value> = self
            .constraints
            .iter()
            .filter(|c| !c.text.trim().is_empty())
            .map(TrackItem::constraint_param)
            .collect();
        if !cons.is_empty() {
            p["constraints"] = json!(cons);
        }
        if !self.objective.trim().is_empty() {
            p["objective"] = json!(self.objective.trim());
        }
        p
    }
}

/// The tracked task for a run on a machine, if known.
pub fn task_for_run(app: &App, mi: usize, run: &AgentRun) -> Option<String> {
    app.task_runs
        .get(&(mi, run.id.clone()))
        .cloned()
        .or_else(|| {
            run.task.clone().filter(|t| {
                app.machines[mi]
                    .model
                    .tasks
                    .iter()
                    .any(|x| &x.id == t && x.intent_revision.is_some())
            })
        })
}

/// **Track this work** for the agent in `pane` (palette action and peek `t`).
pub fn open_track(app: &mut App, mi: usize, pane: &str) {
    let Some(run) = app.machines[mi]
        .model
        .runs
        .iter()
        .find(|r| r.pane == pane)
        .cloned()
    else {
        app.toast("no agent in that pane to track");
        return;
    };
    open_track_run(app, mi, &run.id, pane);
}

/// **Track this work** for a specific run (also the **Link run** choice, lane 2C).
pub fn open_track_run(app: &mut App, mi: usize, run: &str, pane: &str) {
    let id = app.next_ui_id();
    let idem = app.new_idempotency_key("track");
    app.track = Some(TrackForm::new(id, mi, run, pane, idem));
    app.mode = Mode::Popup(Popup::Track);
    app.command_on(
        mi,
        "task.sources",
        json!({"run": run, "limit": 10}),
        Pending::Task(Reply::Sources { form: id }),
    );
}

pub fn track_key(app: &mut App, ev: KeyEvent) {
    let Some(mut f) = app.track.take() else {
        return;
    };
    // Link run (lane 2C): choose a verified run instead of an unverified one.
    if f.phase == TrackPhase::Unverified && ev.key != Key::Named(NamedKey::Escape) {
        if crate::tasks_2c::link_key(app, f.clone(), &ev) {
            return;
        }
        app.track = Some(f);
        app.mode = Mode::Popup(Popup::Track);
        return;
    }
    // Suggest task details (15 §2.2 via 14): preview → confirm → fills this form, unsaved.
    if ev.mods.ctrl() && ev.key == Key::Char('g') && f.phase == TrackPhase::Ready {
        app.track = Some(f);
        crate::assist::suggest_task_details(app);
        return;
    }
    match f.key(&ev) {
        FormOutcome::Stay => {
            app.track = Some(f);
            app.mode = Mode::Popup(Popup::Track);
        }
        FormOutcome::Cancel => app.restore_return(),
        FormOutcome::Submit => {
            let params = f.params();
            f.phase = TrackPhase::Submitting;
            f.error = None;
            let mi = f.machine;
            let id = f.id;
            app.track = Some(f);
            app.mode = Mode::Popup(Popup::Track);
            if !app.mutate(
                mi,
                "task.track",
                params,
                Pending::Task(Reply::Track { form: id }),
            ) && let Some(f) = &mut app.track
            {
                f.phase = TrackPhase::Ready;
                f.error = Some("couldn't record the pending operation locally; not sent".into());
            }
        }
    }
}

// ---- intent edit ---------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct CritDraft {
    pub id: Option<String>,
    pub text: String,
    pub required: bool,
    pub evaluation: String,
    pub checks: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IntentField {
    Title,
    Objective,
    Criterion(usize),
    AddCriterion,
    Stop,
    Save,
    SaveSend,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntentForm {
    pub base_revision: u32,
    pub title: String,
    pub objective: String,
    pub criteria: Vec<CritDraft>,
    pub stop: usize,
    pub field: IntentField,
    pub idem: String,
    pub saving: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IntentOutcome {
    Stay,
    Cancel,
    Save { send: bool },
}

impl IntentForm {
    pub fn from_intent(intent: &Value, idem: String) -> Self {
        let stop = STOP_AT
            .iter()
            .position(|(k, _)| *k == st(intent, "stop_at"))
            .unwrap_or(0);
        IntentForm {
            base_revision: intent.get("revision").and_then(Value::as_u64).unwrap_or(0) as u32,
            title: st(intent, "title").into(),
            objective: st(intent, "objective").into(),
            criteria: arr(intent, "criteria")
                .iter()
                .map(|c| CritDraft {
                    id: c.get("id").and_then(Value::as_str).map(str::to_string),
                    text: st(c, "text").into(),
                    required: c.get("required").and_then(Value::as_bool).unwrap_or(true),
                    evaluation: c
                        .get("evaluation")
                        .and_then(Value::as_str)
                        .unwrap_or("human")
                        .into(),
                    checks: arr(c, "check_definition_ids")
                        .iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect(),
                })
                .collect(),
            stop,
            field: IntentField::Title,
            idem,
            saving: false,
            error: None,
        }
    }

    fn fields(&self) -> Vec<IntentField> {
        let mut f = vec![IntentField::Title, IntentField::Objective];
        f.extend((0..self.criteria.len()).map(IntentField::Criterion));
        f.extend([
            IntentField::AddCriterion,
            IntentField::Stop,
            IntentField::Save,
            IntentField::SaveSend,
        ]);
        f
    }

    fn step(&mut self, fwd: bool) {
        let f = self.fields();
        let i = f.iter().position(|x| *x == self.field).unwrap_or(0);
        self.field = f[if fwd {
            (i + 1).min(f.len() - 1)
        } else {
            i.saturating_sub(1)
        }];
    }

    pub fn key(&mut self, ev: &KeyEvent) -> IntentOutcome {
        if ev.key == Key::Named(NamedKey::Escape) {
            return IntentOutcome::Cancel;
        }
        if self.saving {
            return IntentOutcome::Stay;
        }
        if ctrl(ev, 's') {
            return IntentOutcome::Save { send: false };
        }
        if let Some(fwd) = is_tab(ev) {
            self.step(fwd);
            return IntentOutcome::Stay;
        }
        match (self.field, ev.key) {
            (_, Key::Named(NamedKey::Down)) => self.step(true),
            (_, Key::Named(NamedKey::Up)) => self.step(false),
            (IntentField::Save, Key::Named(NamedKey::Enter)) => {
                return IntentOutcome::Save { send: false };
            }
            (IntentField::SaveSend, Key::Named(NamedKey::Enter)) => {
                return IntentOutcome::Save { send: true };
            }
            (
                IntentField::Title | IntentField::Objective | IntentField::Stop,
                Key::Named(NamedKey::Enter),
            ) => self.step(true),
            (IntentField::Title, _) => {
                edit(&mut self.title, ev);
            }
            (IntentField::Objective, _) => {
                edit(&mut self.objective, ev);
            }
            (IntentField::Criterion(i), _) if ctrl(ev, 'r') => {
                self.criteria[i].required = !self.criteria[i].required
            }
            (IntentField::Criterion(i), _) if ctrl(ev, 'd') => self.remove(i),
            (IntentField::Criterion(i), Key::Named(NamedKey::Backspace))
                if self.criteria[i].text.is_empty() =>
            {
                self.remove(i)
            }
            (IntentField::Criterion(i), Key::Named(NamedKey::Enter)) => {
                self.criteria.insert(i + 1, CritDraft::new());
                self.field = IntentField::Criterion(i + 1);
            }
            (IntentField::Criterion(i), _) => {
                edit(&mut self.criteria[i].text, ev);
            }
            (IntentField::AddCriterion, Key::Named(NamedKey::Enter) | Key::Char(' ')) => {
                self.criteria.push(CritDraft::new());
                self.field = IntentField::Criterion(self.criteria.len() - 1);
            }
            (IntentField::Stop, Key::Named(NamedKey::Right) | Key::Char(' ' | 'l')) => {
                self.stop = (self.stop + 1) % STOP_AT.len()
            }
            (IntentField::Stop, Key::Named(NamedKey::Left) | Key::Char('h')) => {
                self.stop = (self.stop + STOP_AT.len() - 1) % STOP_AT.len()
            }
            _ => {}
        }
        IntentOutcome::Stay
    }

    fn remove(&mut self, i: usize) {
        if i < self.criteria.len() {
            self.criteria.remove(i);
        }
        self.field = if i == 0 {
            if self.criteria.is_empty() {
                IntentField::AddCriterion
            } else {
                IntentField::Criterion(0)
            }
        } else {
            IntentField::Criterion(i - 1)
        };
    }

    /// `task.intent.update` params (record-only; expected revision guards concurrent edits).
    pub fn params(&self, task: &str) -> Value {
        let criteria: Vec<Value> = self
            .criteria
            .iter()
            .filter(|c| !c.text.trim().is_empty())
            .map(|c| {
                let mut v = json!({"text": c.text.trim(), "required": c.required, "evaluation": c.evaluation});
                if let Some(id) = &c.id {
                    v["id"] = json!(id);
                }
                if !c.checks.is_empty() {
                    v["checks"] = json!(c.checks);
                }
                v
            })
            .collect();
        json!({
            "task": task,
            "expected_revision": self.base_revision,
            "title": self.title.trim(),
            "objective": self.objective.trim(),
            "criteria": criteria,
            "stop_at": STOP_AT[self.stop].0,
            "idempotency_key": self.idem,
        })
    }
}

impl CritDraft {
    fn new() -> Self {
        CritDraft {
            id: None,
            text: String::new(),
            required: true,
            evaluation: "human".into(),
            checks: vec![],
        }
    }
}

/// Deterministic clarification text for a saved revision; the user edits it before sending.
pub fn clarification_text(intent: &Value) -> String {
    let mut s = format!(
        "Task details updated (revision {}): {}\n",
        intent.get("revision").and_then(Value::as_u64).unwrap_or(0),
        st(intent, "title")
    );
    if !st(intent, "objective").is_empty() {
        s.push_str(&format!("{}\n", st(intent, "objective")));
    }
    let crit = arr(intent, "criteria");
    if !crit.is_empty() {
        s.push_str("Acceptance criteria:\n");
        for c in crit {
            let req = if c.get("required").and_then(Value::as_bool).unwrap_or(true) {
                "required"
            } else {
                "optional"
            };
            s.push_str(&format!("- {} ({req})\n", st(c, "text")));
        }
    }
    let stop = st(intent, "stop_at");
    if !stop.is_empty() && stop != "unspecified" {
        s.push_str(&format!("Stop at: {}\n", stop_label(stop)));
    }
    s.trim_end().to_string()
}

// ---- acceptance with exceptions -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ExcEntry {
    pub criterion_id: String,
    pub version: u32,
    pub text: String,
    pub status: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExceptionForm {
    pub entries: Vec<ExcEntry>,
    /// `entries.len()` is the submit button.
    pub field: usize,
    pub intent_revision: u64,
    pub package_revision: u64,
    pub subject_id: String,
    pub subject_label: String,
    pub idem: String,
    pub submitting: bool,
    pub error: Option<String>,
}

impl ExceptionForm {
    /// Required criteria that can't pass need an exception with a reason each (15 §7).
    pub fn from_package(pkg: &Value, idem: String) -> Self {
        let entries = criteria_of(pkg)
            .iter()
            .filter(|c| c.get("required").and_then(Value::as_bool).unwrap_or(true))
            .filter(|c| needs_exception(c))
            .map(|c| ExcEntry {
                criterion_id: crit_id(c),
                version: c.get("version").and_then(Value::as_u64).unwrap_or(1) as u32,
                text: st(c, "text").into(),
                status: st(c, "status").into(),
                reason: String::new(),
            })
            .collect();
        let subject = pkg.get("subject").cloned().unwrap_or(Value::Null);
        ExceptionForm {
            entries,
            field: 0,
            intent_revision: pkg
                .get("intent_revision")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            package_revision: pkg.get("revision").and_then(Value::as_u64).unwrap_or(0),
            subject_id: st(&subject, "id").into(),
            subject_label: subject_label(&subject),
            idem,
            submitting: false,
            error: None,
        }
    }

    pub fn complete(&self) -> bool {
        self.entries.iter().all(|e| !e.reason.trim().is_empty())
    }

    pub fn key(&mut self, ev: &KeyEvent) -> FormOutcome {
        if ev.key == Key::Named(NamedKey::Escape) {
            return FormOutcome::Cancel;
        }
        if self.submitting {
            return FormOutcome::Stay;
        }
        let n = self.entries.len();
        if let Some(fwd) = is_tab(ev) {
            self.field = if fwd {
                (self.field + 1).min(n)
            } else {
                self.field.saturating_sub(1)
            };
            return FormOutcome::Stay;
        }
        match ev.key {
            Key::Named(NamedKey::Down) => self.field = (self.field + 1).min(n),
            Key::Named(NamedKey::Up) => self.field = self.field.saturating_sub(1),
            Key::Named(NamedKey::Enter) if self.field == n => {
                if self.complete() {
                    return FormOutcome::Submit;
                }
                self.error = Some("every listed criterion needs a reason".into());
            }
            Key::Named(NamedKey::Enter) => self.field = (self.field + 1).min(n),
            _ if self.field < n => {
                edit(&mut self.entries[self.field].reason, ev);
            }
            _ => {}
        }
        FormOutcome::Stay
    }

    pub fn params(&self, task: &str) -> Value {
        json!({
            "task": task,
            "expected_intent_revision": self.intent_revision,
            "expected_package_revision": self.package_revision,
            "subject_id": self.subject_id,
            "exceptions": self.entries.iter().map(|e| json!({
                "criterion_id": e.criterion_id, "version": e.version, "reason": e.reason.trim()
            })).collect::<Vec<_>>(),
            "idempotency_key": self.idem,
        })
    }
}

fn crit_id(c: &Value) -> String {
    let id = st(c, "criterion_id");
    if id.is_empty() {
        st(c, "id").into()
    } else {
        id.into()
    }
}

fn needs_exception(c: &Value) -> bool {
    if let Some(b) = c.get("needs_exception").and_then(Value::as_bool) {
        return b;
    }
    match st(c, "status") {
        "supported" => false,
        "needs_judgment" => st(c, "evaluation") == "check",
        _ => true,
    }
}

fn criteria_of(pkg: &Value) -> &[Value] {
    let a = arr(pkg, "criteria");
    if a.is_empty() {
        arr(pkg, "assessments")
    } else {
        a
    }
}

pub(crate) fn subject_label(s: &Value) -> String {
    let kind = st(s, "kind");
    let head = st(s, "head_sha");
    let short = &head[..head.len().min(8)];
    match kind {
        "committed" => format!("revision {short}"),
        "dirty" | "checkout_live" => format!("uncommitted changes on {short}"),
        "dirty_snapshot" => format!("snapshot of uncommitted work on {short}"),
        "" if !short.is_empty() => format!("revision {short}"),
        "" => "no captured subject".into(),
        k => format!("{k} {short}"),
    }
}

fn accept_capable(pkg: &Value) -> bool {
    let s = pkg.get("subject").cloned().unwrap_or(Value::Null);
    s.get("accept_capable")
        .and_then(Value::as_bool)
        .or_else(|| {
            // T4 packages say so at the top level; a validated snapshot is accept-capable.
            (st(&s, "kind") == "dirty_snapshot")
                .then(|| pkg.get("accept_capable").and_then(Value::as_bool))
                .flatten()
        })
        .unwrap_or(matches!(st(&s, "kind"), "committed" | "dirty_snapshot"))
}

// ---- messages ----------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum MsgPhase {
    Compose,
    Preparing,
    Prepared {
        message: String,
        recipient: String,
        unsafe_reason: Option<String>,
    },
    Sending {
        message: String,
    },
    /// `send_unsafe`: zero bytes written.
    Refused {
        message: String,
        reason: String,
        pane: Option<String>,
    },
    /// Confirm a manual retry after an unknown/failed outcome.
    ConfirmRetry {
        message: String,
        despite_unknown: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MessageFlow {
    pub text: String,
    pub communicates_intent: bool,
    pub phase: MsgPhase,
    pub idem_prepare: String,
    pub error: Option<String>,
}

// ---- task view -----------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Api {
    Loading,
    Unsupported,
    Ok(Value),
    Err(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TaskSub {
    None,
    Edit(IntentForm),
    Exceptions(ExceptionForm),
    Message(MessageFlow),
    CheckPick {
        sel: usize,
    },
    Authorize(AuthDialog),
    /// T4 screens (`crate::tasks_t4`).
    T4(crate::tasks_t4::T4Sub),
    /// Lane 2C screens (`crate::tasks_2c`): human review, select changes.
    Lane2c(crate::tasks_2c::Sub),
}

/// The check authorization dialog (15 §6.3). It freezes the exact subject and check-definition
/// digest it shows; confirming authorizes those, never whatever the package says by then. A
/// refresh that installs a different subject or definition invalidates it.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthDialog {
    pub check: Value,
    /// Subject id shown (and sent on confirm).
    pub subject: String,
    /// Human label of that subject, frozen with it.
    pub subject_label: String,
    /// Definition digest shown (and sent on confirm as `definition_digest`).
    pub digest: String,
    /// The candidate or definition changed while the dialog was open: confirm is refused.
    pub stale: bool,
}

pub const CANDIDATE_CHANGED: &str = "The candidate changed — review again";

fn check_digest(c: &Value) -> String {
    let d = c
        .get("definition")
        .map(|d| st(d, "definition_digest"))
        .unwrap_or("");
    if d.is_empty() {
        st(c, "definition_digest").to_string()
    } else {
        d.to_string()
    }
}

fn subject_id_of(container: &Value) -> String {
    match container.get("subject") {
        Some(Value::String(s)) => s.clone(),
        Some(s @ Value::Object(_)) => st(s, "id").to_string(),
        _ => String::new(),
    }
}

impl AuthDialog {
    fn new(check: Value, pkg: Option<&Value>) -> Self {
        let subj = pkg
            .and_then(|p| p.get("subject"))
            .cloned()
            .unwrap_or(Value::Null);
        AuthDialog {
            digest: check_digest(&check),
            subject: st(&subj, "id").to_string(),
            subject_label: subject_label(&subj),
            check,
            stale: false,
        }
    }

    /// Validate against a refreshed listing (a review package or a `task.check.list` reply):
    /// the subject and this check's definition digest must be the ones shown.
    fn revalidate(&mut self, container: &Value) {
        if self.stale {
            return;
        }
        let id = st(&self.check, "id");
        let same_def = arr(container, "checks")
            .iter()
            .find(|c| st(c, "id") == id)
            .is_some_and(|c| check_digest(c) == self.digest);
        if subject_id_of(container) != self.subject || !same_def {
            self.stale = true;
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskView {
    pub id: u64,
    pub machine: usize,
    pub task: String,
    pub detail: Option<Value>,
    pub detail_err: Option<String>,
    pub review: Api,
    pub checks: Api,
    pub scroll: u16,
    pub sub: TaskSub,
    pub notice: Option<String>,
    pub watch: HashSet<String>,
    pub last_poll: Option<Instant>,
    pub last_rev: u64,
    pub last_fetch: Option<Instant>,
    /// T4 state (notes, dependencies, model estimate): `crate::tasks_t4`.
    pub t4: crate::tasks_t4::T4State,
}

/// Open the task detail view (from the inbox, goto, peek or a palette action).
pub fn open_task(app: &mut App, mi: usize, task: &str) {
    let id = app.next_ui_id();
    let rev = app.machines[mi]
        .model
        .tasks
        .iter()
        .find(|t| t.id == task)
        .map(|t| t.rev)
        .unwrap_or(0);
    app.task_view = Some(TaskView {
        id,
        machine: mi,
        task: task.into(),
        detail: None,
        detail_err: None,
        review: Api::Loading,
        checks: Api::Loading,
        scroll: 0,
        sub: TaskSub::None,
        notice: None,
        watch: HashSet::new(),
        last_poll: None,
        last_rev: rev,
        last_fetch: None,
        t4: Default::default(),
    });
    app.mode = Mode::Popup(Popup::Task);
    fetch(app, true);
}

pub(crate) fn fetch(app: &mut App, all: bool) {
    let Some(v) = &mut app.task_view else {
        return;
    };
    v.last_fetch = Some(Instant::now());
    let (mi, id, task) = (v.machine, v.id, v.task.clone());
    let review_supported = !matches!(v.review, Api::Unsupported);
    let checks_supported = !matches!(v.checks, Api::Unsupported);
    app.command_on(
        mi,
        "task.detail",
        json!({"task": task}),
        Pending::Task(Reply::Detail { view: id }),
    );
    if all || review_supported {
        app.command_on(
            mi,
            "task.review.get",
            json!({"task": task}),
            Pending::Task(Reply::Review { view: id }),
        );
    }
    if all || checks_supported {
        app.command_on(
            mi,
            "task.check.list",
            json!({"task": task}),
            Pending::Task(Reply::Checks { view: id }),
        );
    }
}

/// A refreshed listing replaced what the authorization dialog shows: invalidate it visibly.
fn revalidate_dialog(v: &mut TaskView, container: &Value) {
    if let TaskSub::Authorize(d) = &mut v.sub {
        let was = d.stale;
        d.revalidate(container);
        if d.stale && !was {
            v.notice = Some(CANDIDATE_CHANGED.into());
        }
    }
}

pub(crate) fn view_of(app: &mut App, id: u64) -> Option<&mut TaskView> {
    app.task_view.as_mut().filter(|v| v.id == id)
}

pub(crate) fn package(v: &TaskView) -> Option<&Value> {
    match &v.review {
        Api::Ok(p) => Some(p.get("package").unwrap_or(p)),
        _ => None,
    }
}

/// Defined checks: from `task.check.list`, else the package.
fn defined_checks(v: &TaskView) -> Vec<Value> {
    if let Api::Ok(c) = &v.checks {
        let a = arr(c, "checks");
        if !a.is_empty() {
            return a.to_vec();
        }
    }
    package(v)
        .map(|p| arr(p, "checks").to_vec())
        .unwrap_or_default()
}

fn active_binding(d: &Value) -> Option<&Value> {
    arr(d, "bindings")
        .iter()
        .rev()
        .find(|b| st(b, "state") == "active")
}

fn suspended_binding(d: &Value) -> Option<&Value> {
    let has_active = active_binding(d).is_some();
    arr(d, "bindings")
        .iter()
        .rev()
        .find(|b| st(b, "state") == "suspended" && !has_active)
}

fn run_info<'a>(d: &'a Value, run: &str) -> Option<&'a Value> {
    arr(d, "runs").iter().find(|r| st(r, "id") == run)
}

fn run_pane(app: &App, mi: usize, d: &Value, run: &str) -> Option<String> {
    run_info(d, run)
        .map(|r| st(r, "pane").to_string())
        .filter(|p| !p.is_empty())
        .or_else(|| {
            app.machines[mi]
                .model
                .runs
                .iter()
                .find(|r| r.id == run)
                .map(|r| r.pane.clone())
        })
}

fn run_desc(d: &Value, run: &str) -> String {
    match run_info(d, run) {
        Some(r) => format!("{} #{}", st(r, "harness"), st(r, "handle")),
        None => format!("run {}", truncate(run, 10)),
    }
}

pub fn task_key(app: &mut App, ev: KeyEvent) {
    let Some(mut v) = app.task_view.take() else {
        return;
    };
    app.mode = Mode::Popup(Popup::Task);
    let mi = v.machine;
    let task = v.task.clone();
    match std::mem::replace(&mut v.sub, TaskSub::None) {
        TaskSub::Edit(mut f) => {
            match f.key(&ev) {
                IntentOutcome::Stay => v.sub = TaskSub::Edit(f),
                IntentOutcome::Cancel => v.notice = Some("Draft discarded".into()),
                IntentOutcome::Save { send } => {
                    let params = f.params(&task);
                    f.saving = true;
                    f.error = None;
                    v.sub = TaskSub::Edit(f);
                    let id = v.id;
                    app.task_view = Some(v);
                    if !app.mutate(
                        mi,
                        "task.intent.update",
                        params,
                        Pending::Task(Reply::IntentSaved { view: id, send }),
                    ) && let Some(v) = &mut app.task_view
                        && let TaskSub::Edit(f) = &mut v.sub
                    {
                        f.saving = false;
                        f.error = Some("couldn't record the pending operation locally".into());
                    }
                    return;
                }
            }
            app.task_view = Some(v);
            return;
        }
        TaskSub::Exceptions(mut f) => {
            match f.key(&ev) {
                FormOutcome::Stay => v.sub = TaskSub::Exceptions(f),
                FormOutcome::Cancel => {}
                FormOutcome::Submit => {
                    let params = f.params(&task);
                    f.submitting = true;
                    v.sub = TaskSub::Exceptions(f);
                    let id = v.id;
                    app.task_view = Some(v);
                    app.mutate(
                        mi,
                        "task.review.accept",
                        params,
                        Pending::Task(Reply::Accepted { view: id }),
                    );
                    return;
                }
            }
            app.task_view = Some(v);
            return;
        }
        TaskSub::Message(mut m) => {
            message_key(app, &mut v, &mut m, ev);
            if matches!(app.mode, Mode::Normal) {
                // Open pane to send: the view closes, focus moved by explicit request.
                app.task_view = None;
                return;
            }
            app.task_view = Some(v);
            return;
        }
        TaskSub::CheckPick { sel } => {
            let checks = defined_checks(&v);
            match ev.key {
                Key::Named(NamedKey::Escape) => {}
                Key::Char('j') | Key::Named(NamedKey::Down) => {
                    v.sub = TaskSub::CheckPick {
                        sel: (sel + 1).min(checks.len().saturating_sub(1)),
                    }
                }
                Key::Char('k') | Key::Named(NamedKey::Up) => {
                    v.sub = TaskSub::CheckPick {
                        sel: sel.saturating_sub(1),
                    }
                }
                Key::Named(NamedKey::Enter) => {
                    if let Some(c) = checks.get(sel).cloned() {
                        app.task_view = Some(v);
                        run_check(app, c);
                        return;
                    }
                }
                _ => v.sub = TaskSub::CheckPick { sel },
            }
            app.task_view = Some(v);
            return;
        }
        TaskSub::Authorize(d) => {
            match ev.key {
                Key::Char('y' | 'Y') if d.stale => {
                    v.notice = Some(CANDIDATE_CHANGED.into());
                    v.sub = TaskSub::Authorize(d);
                }
                Key::Char('y' | 'Y') => {
                    app.task_view = Some(v);
                    submit_check(app, &d.check, &d.subject, &d.digest, true);
                    return;
                }
                Key::Named(NamedKey::Escape) | Key::Char('n' | 'N') => {
                    v.notice = Some("Check not run".into())
                }
                _ => v.sub = TaskSub::Authorize(d),
            }
            app.task_view = Some(v);
            return;
        }
        TaskSub::T4(s) => {
            app.task_view = Some(v);
            crate::tasks_t4::sub_key(app, s, ev);
            return;
        }
        TaskSub::Lane2c(s) => {
            app.task_view = Some(v);
            crate::tasks_2c::sub_key(app, s, ev);
            return;
        }
        TaskSub::None => {}
    }
    let detail = v.detail.clone().unwrap_or(Value::Null);
    v.notice = None;
    if crate::tasks_t4::is_main_key(&ev) {
        app.task_view = Some(v);
        crate::tasks_t4::main_key(app, ev);
        return;
    }
    if crate::tasks_2c::is_main_key(&ev) {
        app.task_view = Some(v);
        crate::tasks_2c::main_key(app, ev);
        return;
    }
    match ev.key {
        Key::Named(NamedKey::Escape) | Key::Char('q') => {
            app.restore_return();
            return;
        }
        Key::Char('j') | Key::Named(NamedKey::Down) => v.scroll = v.scroll.saturating_add(1),
        Key::Char('k') | Key::Named(NamedKey::Up) => v.scroll = v.scroll.saturating_sub(1),
        Key::Named(NamedKey::PageDown) | Key::Char(' ') => v.scroll = v.scroll.saturating_add(10),
        Key::Named(NamedKey::PageUp) => v.scroll = v.scroll.saturating_sub(10),
        Key::Char('g') => v.scroll = 0,
        Key::Char('r') => {
            app.task_view = Some(v);
            fetch(app, false);
            return;
        }
        Key::Char('e') => match detail.get("intent").filter(|i| !i.is_null()) {
            Some(intent) => {
                let idem = app.new_idempotency_key("intent");
                v.sub = TaskSub::Edit(IntentForm::from_intent(intent, idem));
            }
            None => v.notice = Some("This task has no confirmed intent yet".into()),
        },
        Key::Char('c') => match suspended_binding(&detail) {
            Some(b) => {
                let run = st(b, "run_id").to_string();
                let idem = app.new_idempotency_key("bind");
                let id = v.id;
                v.notice = Some("Continuing the task in the new conversation…".into());
                app.task_view = Some(v);
                app.mutate(
                    mi,
                    "task.bind",
                    json!({"task": task, "run": run, "idempotency_key": idem}),
                    Pending::Task(Reply::Bound { view: id }),
                );
                return;
            }
            None => v.notice = Some("No suspended binding to continue".into()),
        },
        Key::Char('n') => match suspended_binding(&detail) {
            Some(b) => {
                let run = st(b, "run_id").to_string();
                if let Some(p) = run_pane(app, mi, &detail, &run) {
                    app.task_view = Some(v);
                    app.return_to.push(Popup::Task);
                    open_track(app, mi, &p);
                    return;
                }
                v.notice = Some("That run is gone".into());
            }
            None => v.notice = Some("Track new work is offered after a conversation change".into()),
        },
        Key::Char('w') => {
            if active_binding(&detail).is_none() {
                v.notice = Some("No active run binding to message".into());
            } else {
                let idem = app.new_idempotency_key("msg");
                v.sub = TaskSub::Message(MessageFlow {
                    text: String::new(),
                    communicates_intent: false,
                    phase: MsgPhase::Compose,
                    idem_prepare: idem,
                    error: None,
                });
            }
        }
        Key::Char('o') => {
            let pane = active_binding(&detail)
                .or_else(|| arr(&detail, "bindings").last())
                .and_then(|b| run_pane(app, mi, &detail, st(b, "run_id")));
            match pane {
                Some(p) => {
                    app.focus_pane(mi, &p);
                    app.mode = Mode::Normal;
                    return;
                }
                None => v.notice = Some("No pane for this task".into()),
            }
        }
        Key::Char('v') => {
            if matches!(v.review, Api::Unsupported) && matches!(v.checks, Api::Unsupported) {
                v.notice = Some("Checks need a newer server on this machine".into());
            } else {
                let checks = defined_checks(&v);
                match checks.len() {
                    0 => {
                        v.notice = Some(
                            "No defined checks — choose verification or ask the agent to verify"
                                .into(),
                        )
                    }
                    1 => {
                        app.task_view = Some(v);
                        run_check(app, checks[0].clone());
                        return;
                    }
                    _ => v.sub = TaskSub::CheckPick { sel: 0 },
                }
            }
        }
        // Summarize review (14): preview → confirm → editable text.
        Key::Char('S') => {
            app.task_view = Some(v);
            crate::assist::summarize_review(app, mi, &task);
            return;
        }
        // Drafts for this task (08 §6.7).
        Key::Char('D') => {
            let title = detail
                .pointer("/task/title")
                .and_then(Value::as_str)
                .unwrap_or(&task)
                .to_string();
            app.task_view = Some(v);
            crate::drafts::open_task(app, mi, &task, &title);
            return;
        }
        Key::Char('m') => match package(&v) {
            None => {
                v.notice = Some(match v.review {
                    Api::Unsupported => "Review needs a newer server on this machine".into(),
                    _ => "No review package yet".into(),
                })
            }
            Some(p) if !accept_capable(p) => {
                v.notice = Some("Select a committed revision to record acceptance".into())
            }
            Some(p) => {
                let idem = app.new_idempotency_key("accept");
                v.sub = TaskSub::Exceptions(ExceptionForm::from_package(p, idem));
            }
        },
        _ => {}
    }
    app.task_view = Some(v);
}

/// Run a defined check; the first run for a candidate needs explicit authorization, which
/// opens the dialog (freezing subject + definition digest) instead of sending anything.
fn run_check(app: &mut App, check: Value) {
    let Some(v) = &mut app.task_view else {
        return;
    };
    let auth = check.get("authorization").cloned().unwrap_or(Value::Null);
    let status = st(&auth, "status");
    if status == "unavailable" {
        v.notice = Some(format!("Can't run: {}", st(&auth, "reason")));
        return;
    }
    if status != "authorized" {
        v.sub = TaskSub::Authorize(AuthDialog::new(check, package(v)));
        return;
    }
    let subject = package(v).map(subject_id_of).unwrap_or_default();
    let digest = check_digest(&check);
    submit_check(app, &check, &subject, &digest, false);
}

/// Submit `task.check.run` for exactly `subject` and the definition `digest` the user saw;
/// with `authorize`, this is the confirmed per-candidate authorization too.
fn submit_check(app: &mut App, check: &Value, subject: &str, digest: &str, authorize: bool) {
    let Some(v) = &mut app.task_view else {
        return;
    };
    let auth = check.get("authorization").cloned().unwrap_or(Value::Null);
    let check_id = st(check, "id").to_string();
    let (mi, id, task) = (v.machine, v.id, v.task.clone());
    v.notice = Some(format!("Submitting check {}…", st(check, "name")));
    let mut params = json!({"task": task, "check": check_id});
    if !subject.is_empty() {
        params["subject"] = json!(subject);
    }
    if !digest.is_empty() {
        params["definition_digest"] = json!(digest);
    }
    if authorize {
        params["authorize"] = json!(true);
        let label = st(&auth, "confirmation_label");
        params["confirmation_label"] = json!(if label.is_empty() {
            "Runs code modified by this task"
        } else {
            label
        });
    }
    params["idempotency_key"] = json!(app.new_idempotency_key("check"));
    app.mutate(
        mi,
        "task.check.run",
        params,
        Pending::Task(Reply::CheckRun {
            view: id,
            subject: subject.to_string(),
        }),
    );
}

fn message_key(app: &mut App, v: &mut TaskView, m: &mut MessageFlow, ev: KeyEvent) {
    let mi = v.machine;
    let task = v.task.clone();
    v.sub = TaskSub::None;
    let keep = |v: &mut TaskView, m: &MessageFlow| v.sub = TaskSub::Message(m.clone());
    match m.phase.clone() {
        MsgPhase::Compose => match ev.key {
            Key::Named(NamedKey::Escape) => {}
            Key::Named(NamedKey::Enter) if ev.mods.alt() => {
                m.text.push('\n');
                keep(v, m);
            }
            Key::Named(NamedKey::Enter) => {
                if m.text.trim().is_empty() {
                    keep(v, m);
                    return;
                }
                m.phase = MsgPhase::Preparing;
                keep(v, m);
                let params = json!({"task": task, "text": m.text.trim(), "communicates_intent": m.communicates_intent, "idempotency_key": m.idem_prepare});
                let id = v.id;
                app.mutate(
                    mi,
                    "task.message.prepare",
                    params,
                    Pending::Task(Reply::Prepared { view: id }),
                );
            }
            _ => {
                edit(&mut m.text, &ev);
                keep(v, m);
            }
        },
        MsgPhase::Preparing | MsgPhase::Sending { .. } => match ev.key {
            Key::Named(NamedKey::Escape) => {
                // The send continues on the server; its outcome stays visible in Messages.
                if let MsgPhase::Sending { message } = &m.phase {
                    v.watch.insert(message.clone());
                }
            }
            _ => keep(v, m),
        },
        MsgPhase::Prepared {
            message, recipient, ..
        } => match ev.key {
            Key::Named(NamedKey::Enter) | Key::Char('y') => {
                let id = v.id;
                m.phase = MsgPhase::Sending {
                    message: message.clone(),
                };
                keep(v, m);
                let idem = app.new_idempotency_key("send");
                app.mutate(
                    mi,
                    "task.message.send",
                    json!({"message": message, "idempotency_key": idem}),
                    Pending::Task(Reply::Sent { view: id }),
                );
            }
            Key::Char('o') => {
                if let Some(p) = recipient_pane(app, v, &message) {
                    app.focus_pane(mi, &p);
                    app.mode = Mode::Normal;
                } else {
                    keep(v, m);
                }
            }
            Key::Named(NamedKey::Escape) | Key::Char('c') => {
                app.command_on(
                    mi,
                    "task.message.cancel",
                    json!({"message": message}),
                    Pending::Ignore,
                );
                v.notice = Some(format!("Message to {recipient} cancelled (not sent)"));
            }
            _ => keep(v, m),
        },
        MsgPhase::Refused { message, pane, .. } => match ev.key {
            Key::Char('o') => {
                if let Some(p) = pane.or_else(|| recipient_pane(app, v, &message)) {
                    app.focus_pane(mi, &p);
                    app.mode = Mode::Normal;
                } else {
                    keep(v, m);
                }
            }
            Key::Named(NamedKey::Escape) => {}
            _ => keep(v, m),
        },
        MsgPhase::ConfirmRetry {
            message,
            despite_unknown,
        } => match ev.key {
            Key::Char('y' | 'Y') => {
                let id = v.id;
                m.phase = MsgPhase::Sending {
                    message: message.clone(),
                };
                keep(v, m);
                let mut p =
                    json!({"message": message, "idempotency_key": app.new_idempotency_key("send")});
                if despite_unknown {
                    p["retry_despite_unknown"] = json!(true);
                } else {
                    p["retry"] = json!(true);
                }
                app.mutate(
                    mi,
                    "task.message.send",
                    p,
                    Pending::Task(Reply::Sent { view: id }),
                );
            }
            Key::Named(NamedKey::Escape) | Key::Char('n' | 'N') => {}
            _ => keep(v, m),
        },
    }
}

fn recipient_pane(app: &App, v: &TaskView, message: &str) -> Option<String> {
    let d = v.detail.as_ref()?;
    let run = arr(d, "messages")
        .iter()
        .find(|m| st(m, "id") == message)
        .map(|m| st(m, "run").to_string())
        .or_else(|| active_binding(d).map(|b| st(b, "run_id").to_string()))?;
    run_pane(app, v.machine, d, &run)
}

/// Retry a message whose last outcome is unknown or failed (explicit, warns).
pub fn message_state(d: &Value, id: &str) -> Option<(String, Option<String>)> {
    arr(d, "messages")
        .iter()
        .find(|m| st(m, "id") == id)
        .map(|m| {
            (
                st(m, "state").to_string(),
                m.get("detail").and_then(Value::as_str).map(str::to_string),
            )
        })
}

// ---- replies ------------------------------------------------------------------------------------------

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    app.dirty = true;
    match r {
        Reply::Sources { form } => {
            let Some(f) = app.track.as_mut().filter(|f| f.id == form) else {
                return;
            };
            let mut link = false;
            match res {
                Ok(v) => f.load_sources(&v),
                Err(e) if e.reason() == Some("binding_unverified") => {
                    f.phase = TrackPhase::Unverified;
                    link = true;
                }
                Err(e) if e.is_method_not_found() => {
                    f.phase = TrackPhase::Unverified;
                    f.error = Some("Tracking needs a newer server on this machine".into());
                }
                Err(e) => {
                    f.phase = TrackPhase::Unverified;
                    f.error = Some(e.message);
                }
            }
            if link {
                crate::tasks_2c::request_link(app, mi);
            }
        }
        Reply::Track { form } => {
            let open = app.track.as_ref().is_some_and(|f| f.id == form);
            match res {
                Ok(v) => {
                    let task = v.get("task").cloned().unwrap_or(Value::Null);
                    let tid = st(&task, "id").to_string();
                    if let Some(b) = v.get("binding") {
                        app.task_runs
                            .insert((mi, st(b, "run_id").to_string()), tid.clone());
                    }
                    if open {
                        app.track = None;
                        app.restore_return();
                    }
                    app.toast(format!(
                        "Tracking “{}” · #{} — saving sent nothing to the agent",
                        truncate(st(&task, "title"), 40),
                        st(&task, "handle")
                    ));
                }
                Err(e) => {
                    let mut link = false;
                    if let Some(f) = app.track.as_mut().filter(|f| f.id == form) {
                        if e.reason() == Some("binding_unverified") {
                            f.phase = TrackPhase::Unverified;
                            link = true;
                        } else {
                            f.phase = TrackPhase::Ready;
                            f.error = Some(e.message);
                        }
                    } else {
                        app.toast(format!("✗ track: {}", e.message));
                    }
                    if link {
                        crate::tasks_2c::request_link(app, mi);
                    }
                }
            }
        }
        Reply::Detail { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            match res {
                Ok(d) => {
                    // Messages still sending are polled until they settle.
                    for m in arr(&d, "messages") {
                        if st(m, "state") == "sending" {
                            v.watch.insert(st(m, "id").to_string());
                        }
                    }
                    v.detail = Some(d);
                    v.detail_err = None;
                }
                Err(e) => v.detail_err = Some(e.message),
            }
        }
        Reply::Review { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            v.review = match res {
                Ok(p) => Api::Ok(p),
                Err(e) if e.is_method_not_found() => Api::Unsupported,
                Err(e) => Api::Err(e.message),
            };
            if let Some(p) = package(v).cloned() {
                revalidate_dialog(v, &p);
            }
        }
        Reply::Checks { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            v.checks = match res {
                Ok(p) => Api::Ok(p),
                Err(e) if e.is_method_not_found() => Api::Unsupported,
                Err(e) => Api::Err(e.message),
            };
            if let Api::Ok(c) = &v.checks
                && !arr(c, "checks").is_empty()
            {
                let c = c.clone();
                revalidate_dialog(v, &c);
            }
        }
        Reply::IntentSaved { view, send } => {
            let msg_key = app.new_idempotency_key("msg");
            let Some(v) = view_of(app, view) else {
                return;
            };
            match res {
                Ok(r) => {
                    let intent = r.get("intent").cloned().unwrap_or(Value::Null);
                    let rev = intent.get("revision").and_then(Value::as_u64).unwrap_or(0);
                    let note = r
                        .get("note")
                        .and_then(Value::as_str)
                        .map(|n| format!(" · {n}"))
                        .unwrap_or_default();
                    v.notice = Some(format!("Saved revision {rev}{note}"));
                    v.sub = if send {
                        TaskSub::Message(MessageFlow {
                            text: clarification_text(&intent),
                            communicates_intent: true,
                            phase: MsgPhase::Compose,
                            idem_prepare: msg_key,
                            error: None,
                        })
                    } else {
                        TaskSub::None
                    };
                    fetch(app, false);
                }
                Err(e) => {
                    if let TaskSub::Edit(f) = &mut v.sub {
                        f.saving = false;
                        f.error = Some(if e.reason() == Some("review_changed") {
                            let cur = e
                                .details
                                .get("current")
                                .and_then(Value::as_u64)
                                .map(|c| format!(" (now revision {c})"))
                                .unwrap_or_default();
                            format!(
                                "Intent changed elsewhere{cur}; your draft is kept — Esc and reopen to see it"
                            )
                        } else {
                            e.message
                        });
                        // A definitive refusal: the next save is a new request.
                        f.idem = format!("{}-r{}", f.idem, now_ms());
                    }
                }
            }
        }
        Reply::Prepared { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            let TaskSub::Message(m) = &mut v.sub else {
                return;
            };
            match res {
                Ok(r) => {
                    let msg = r.get("message").cloned().unwrap_or(Value::Null);
                    let run = r
                        .get("recipient")
                        .map(|x| st(x, "run").to_string())
                        .unwrap_or_else(|| st(&msg, "run").to_string());
                    let d = v.detail.clone().unwrap_or(Value::Null);
                    m.text = st(&msg, "text").to_string();
                    m.phase = MsgPhase::Prepared {
                        message: st(&msg, "id").to_string(),
                        recipient: run_desc(&d, &run),
                        unsafe_reason: r.get("unsafe").and_then(Value::as_str).map(str::to_string),
                    };
                }
                Err(e) => {
                    m.phase = MsgPhase::Compose;
                    m.error = Some(e.message);
                    m.idem_prepare = format!("{}-r{}", m.idem_prepare, now_ms());
                }
            }
        }
        Reply::Sent { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            let TaskSub::Message(m) = &mut v.sub else {
                if let Err(e) = res {
                    v.notice = Some(format!("Not sent: {}", e.message));
                }
                return;
            };
            let message = match &m.phase {
                MsgPhase::Sending { message } => message.clone(),
                _ => String::new(),
            };
            match res {
                Ok(r) => {
                    let id = r
                        .get("message")
                        .map(|x| st(x, "id").to_string())
                        .unwrap_or(message);
                    v.watch.insert(id);
                    v.last_poll = None;
                }
                Err(e) if e.reason() == Some("send_unsafe") => {
                    m.phase = MsgPhase::Refused {
                        message,
                        reason: e
                            .details
                            .get("detail")
                            .and_then(Value::as_str)
                            .unwrap_or(&e.message)
                            .to_string(),
                        pane: e
                            .details
                            .get("pane")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    };
                }
                Err(e) if e.reason() == Some("delivery_unknown") => {
                    m.phase = MsgPhase::ConfirmRetry {
                        message,
                        despite_unknown: true,
                    };
                }
                Err(e) => {
                    m.error = Some(e.message);
                    m.phase = MsgPhase::ConfirmRetry {
                        message,
                        despite_unknown: false,
                    };
                }
            }
        }
        Reply::MessageGet { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            let Ok(r) = res else {
                return;
            };
            let msg = r.get("message").cloned().unwrap_or(Value::Null);
            let id = st(&msg, "id").to_string();
            let state = st(&msg, "state").to_string();
            if let Some(d) = &mut v.detail
                && let Some(list) = d.get_mut("messages").and_then(Value::as_array_mut)
            {
                match list.iter_mut().find(|m| st(m, "id") == id) {
                    Some(slot) => *slot = msg.clone(),
                    None => list.push(msg.clone()),
                }
            }
            if !matches!(state.as_str(), "sending" | "prepared") {
                v.watch.remove(&id);
                if let TaskSub::Message(m) = &mut v.sub
                    && matches!(&m.phase, MsgPhase::Sending { message } if *message == id)
                {
                    match state.as_str() {
                        "delivered" => {
                            v.sub = TaskSub::None;
                            v.notice = Some(
                                "✓ Delivered — a receipt is not proof the agent understood".into(),
                            );
                        }
                        "delivery_unknown" => {
                            m.phase = MsgPhase::ConfirmRetry {
                                message: id.clone(),
                                despite_unknown: true,
                            };
                        }
                        "failed" => {
                            m.error = msg
                                .get("detail")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            m.phase = MsgPhase::ConfirmRetry {
                                message: id.clone(),
                                despite_unknown: false,
                            };
                        }
                        _ => {}
                    }
                }
                fetch(app, false);
            }
        }
        Reply::Bound { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            match res {
                Ok(r) => {
                    v.notice = Some(if r.get("pending").and_then(Value::as_bool) == Some(true) {
                        "Switch queued for the next turn boundary".into()
                    } else {
                        "Continuing the task in this conversation".into()
                    });
                    fetch(app, false);
                }
                Err(e) => v.notice = Some(format!("Couldn't continue: {}", e.message)),
            }
        }
        Reply::Accepted { view } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            match res {
                Ok(r) => {
                    let n = r
                        .get("acceptance")
                        .map(|a| arr(a, "exceptions").len())
                        .unwrap_or(0);
                    v.sub = TaskSub::None;
                    v.notice = Some(if n > 0 {
                        "Reviewed with exceptions — nothing was merged or finished".into()
                    } else {
                        "Reviewed — nothing was merged or finished".into()
                    });
                    fetch(app, true);
                }
                Err(e) => {
                    let msg = if e.reason() == Some("review_changed") {
                        "The review changed since you opened it — refreshed; inspect it again"
                            .to_string()
                    } else {
                        e.message.clone()
                    };
                    v.sub = TaskSub::None;
                    v.notice = Some(msg);
                    fetch(app, true);
                }
            }
        }
        Reply::CheckRun { view, subject } => {
            let Some(v) = view_of(app, view) else {
                return;
            };
            match res {
                Ok(_) => {
                    v.notice = Some("Check submitted; results appear under Check runs".into());
                    fetch(app, false);
                }
                Err(e) if e.reason() == Some("authorization_required") => {
                    let def = e.details.get("definition").cloned().unwrap_or(Value::Null);
                    let mut check = e.details.get("check").cloned().unwrap_or_else(|| {
                        json!({"id": st(&def, "id"), "name": st(&def, "name"),
                               "command": def.get("command").cloned().unwrap_or(Value::Null)})
                    });
                    if check.get("definition").is_none() && !def.is_null() {
                        check["definition"] = def;
                    }
                    check["authorization"] = json!({"status": "required",
                        "confirmation_label": e.details.get("confirmation_label").cloned().unwrap_or(json!("Runs code modified by this task")),
                        "reasons": e.details.get("reasons").cloned().unwrap_or(json!([]))});
                    // Frozen to the subject this run was submitted for and the definition the
                    // server reported, not to whatever the package shows later.
                    let mut d = AuthDialog::new(check, package(v));
                    if d.subject != subject {
                        d.subject_label = format!("subject {}", truncate(&subject, 10));
                        d.subject = subject;
                    }
                    v.sub = TaskSub::Authorize(d);
                }
                Err(e) => v.notice = Some(format!("Check not run: {}", e.message)),
            }
        }
        Reply::T4(r) => crate::tasks_t4::on_reply(app, mi, r, res),
        Reply::Lane2c(r) => crate::tasks_2c::on_reply(app, mi, r, res),
        Reply::TaskRuns { task } => {
            let Ok(d) = res else {
                return;
            };
            app.task_runs.retain(|(m, _), t| !(*m == mi && *t == task));
            for b in arr(&d, "bindings") {
                if matches!(st(b, "state"), "active" | "suspended") {
                    app.task_runs
                        .insert((mi, st(b, "run_id").to_string()), task.clone());
                }
            }
        }
    }
}

/// After a model frame: refresh the run → task map for changed tracked tasks, and the open view.
pub fn on_model(app: &mut App, mi: usize) {
    let tasks: Vec<(String, u64)> = app.machines[mi]
        .model
        .tasks
        .iter()
        .filter(|t| t.intent_revision.is_some() && t.status != "archived")
        .map(|t| (t.id.clone(), t.rev))
        .collect();
    for (id, rev) in tasks {
        let k = (mi, id.clone());
        if app.task_runs_rev.get(&k) != Some(&rev) {
            app.task_runs_rev.insert(k, rev);
            app.command_on(
                mi,
                "task.detail",
                json!({"task": id.clone()}),
                Pending::Task(Reply::TaskRuns { task: id }),
            );
        }
    }
    let changed = app.task_view.as_ref().and_then(|v| {
        (v.machine == mi).then(|| {
            app.machines[mi]
                .model
                .tasks
                .iter()
                .find(|t| t.id == v.task)
                .map(|t| t.rev)
        })?
    });
    if let Some(rev) = changed
        && let Some(v) = &mut app.task_view
        && v.last_rev != rev
    {
        v.last_rev = rev;
        if v.last_fetch
            .is_none_or(|t| t.elapsed() >= Duration::from_millis(500))
        {
            fetch(app, false);
        }
    }
}

/// The next poll of messages still sending (only while the task view is open).
pub(crate) fn deadlines(app: &App, now: Instant, d: &mut crate::deadline::Deadlines) {
    if let Some(v) = &app.task_view
        && matches!(app.mode, Mode::Popup(Popup::Task))
        && !v.watch.is_empty()
    {
        d.at(
            "tasks",
            v.last_poll.map_or(now, |t| t + Duration::from_secs(1)),
        );
    }
}

/// Poll messages that are still sending (about once a second while the view is open).
pub fn tick(app: &mut App) {
    let Some(v) = &mut app.task_view else {
        return;
    };
    if !matches!(app.mode, Mode::Popup(Popup::Task)) || v.watch.is_empty() {
        return;
    }
    if v.last_poll
        .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
    {
        return;
    }
    v.last_poll = Some(Instant::now());
    let (mi, id) = (v.machine, v.id);
    let ids: Vec<String> = v.watch.iter().cloned().collect();
    for m in ids {
        app.command_on(
            mi,
            "task.message.get",
            json!({"message": m}),
            Pending::Task(Reply::MessageGet { view: id }),
        );
    }
}

/// The link dropped while a form was submitting: say so instead of spinning.
pub fn on_disconnect(app: &mut App, mi: usize) {
    if let Some(f) = &mut app.track
        && f.machine == mi
        && f.phase == TrackPhase::Submitting
    {
        f.phase = TrackPhase::Ready;
        f.error = Some(
            "Connection lost — outcome unknown. Tracking again reuses the same request key, so it can't create a duplicate".into(),
        );
    }
    if let Some(v) = &mut app.task_view
        && v.machine == mi
    {
        v.notice =
            Some("Machine offline — showing the last observed state; actions disabled".into());
        match &mut v.sub {
            TaskSub::Edit(f) if f.saving => {
                f.saving = false;
                f.error = Some("Connection lost — outcome unknown; it will be reconciled".into());
            }
            TaskSub::Exceptions(f) if f.submitting => {
                f.submitting = false;
                f.error = Some("Connection lost — outcome unknown; it will be reconciled".into());
            }
            _ => {}
        }
    }
}

// ---- drawing ----------------------------------------------------------------------------------------

pub(crate) type Lines = Vec<(String, Style)>;

pub(crate) fn wrap_push(out: &mut Lines, text: &str, indent: &str, w: usize, style: Style) {
    for raw in text.lines() {
        let mut line = String::new();
        for word in raw.split(' ') {
            if !line.is_empty() && line.chars().count() + word.chars().count() + 1 > w {
                out.push((format!("{indent}{line}"), style));
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push((format!("{indent}{line}"), style));
    }
}

pub fn task_lines(app: &App, v: &TaskView, w: usize) -> Lines {
    let t = app.theme;
    let mut out: Lines = Vec::new();
    let Some(d) = &v.detail else {
        out.push((
            v.detail_err
                .clone()
                .unwrap_or_else(|| "loading task…".into()),
            t.dim(),
        ));
        return out;
    };
    let task = d.get("task").cloned().unwrap_or(Value::Null);
    let label = st(&task, "review_label");
    let label_txt = if label.is_empty() {
        "Tracked"
    } else {
        label_text(label)
    };
    out.push((
        format!("{}    {}", st(&task, "title"), label_txt),
        t.bold(t.fg),
    ));
    let mut facts = vec![
        format!("#{}", st(&task, "handle")),
        st(&task, "ownership").to_string(),
        st(&task, "status").to_string(),
    ];
    if let Some(e) = task.get("effort").and_then(Value::as_str) {
        facts.push(format!("effort: {e}"));
    }
    if let Some(p) = task.get("priority").and_then(Value::as_i64) {
        facts.push(format!("priority {p}"));
    }
    if app.machines.len() > 1 {
        facts.push(format!("on {}", app.machines[v.machine].label));
    }
    out.push((facts.join(" · "), t.dim()));
    if st(&task, "ownership") == "attached" {
        out.push((
            "Attached: finishing or archiving never stops the agent or touches its files".into(),
            t.dim(),
        ));
    }
    out.push((String::new(), t.text()));
    // Intent.
    match d.get("intent").filter(|i| !i.is_null()) {
        None => out.push(("No confirmed intent yet".into(), t.dim())),
        Some(intent) => {
            let rev = intent.get("revision").and_then(Value::as_u64).unwrap_or(0);
            out.push((format!("Intent · revision {rev}"), t.bold(t.accent)));
            if !arr(d, "uncommunicated").is_empty() {
                out.push((
                    format!("⚠ Agent has not been told about revision {rev}"),
                    t.bold(t.yellow),
                ));
            }
            if !st(intent, "objective").is_empty() {
                wrap_push(&mut out, st(intent, "objective"), "  ", w, t.text());
            }
            let crit = arr(intent, "criteria");
            if crit.is_empty() {
                out.push(("  No acceptance criteria".into(), t.dim()));
            }
            for c in crit {
                let req = if c.get("required").and_then(Value::as_bool).unwrap_or(true) {
                    "required"
                } else {
                    "optional"
                };
                out.push((
                    format!("  • {}  [{req} · {}]", st(c, "text"), st(c, "evaluation")),
                    t.text(),
                ));
            }
            for c in arr(intent, "constraints") {
                out.push((format!("  constraint: {}", st(c, "text")), t.text()));
            }
            let stop = st(intent, "stop_at");
            let detail = intent
                .get("stop_detail")
                .and_then(Value::as_str)
                .map(|s| format!(" — {s}"))
                .unwrap_or_default();
            out.push((
                format!("  Stop at: {}{detail}", stop_label(stop)),
                if stop == "unspecified" {
                    t.s(t.yellow)
                } else {
                    t.text()
                },
            ));
            if let Some(ex) = intent.get("source_excerpt").and_then(Value::as_str) {
                out.push(("  Requested (verbatim):".into(), t.dim()));
                let lines: Vec<&str> = ex.lines().collect();
                for l in lines.iter().take(8) {
                    wrap_push(&mut out, l, "  │ ", w.saturating_sub(4), t.text());
                }
                if lines.len() > 8 {
                    out.push((format!("  │ … {} more lines", lines.len() - 8), t.dim()));
                }
            }
        }
    }
    out.push((String::new(), t.text()));
    // Bindings.
    out.push(("Runs".into(), t.bold(t.accent)));
    let bs = arr(d, "bindings");
    if bs.is_empty() {
        out.push(("  Not bound to a run".into(), t.dim()));
    }
    for b in bs {
        let run = st(b, "run_id");
        let conv = st(b, "native_conversation_id");
        let state = st(b, "state");
        let range = match b.get("end_turn").and_then(Value::as_u64) {
            Some(e) => format!(
                "turns {}–{}",
                b.get("start_turn").and_then(Value::as_u64).unwrap_or(0),
                e.saturating_sub(1)
            ),
            None => format!(
                "from turn {}",
                b.get("start_turn").and_then(Value::as_u64).unwrap_or(0)
            ),
        };
        let style = match state {
            "active" => t.text(),
            "suspended" => t.s(t.yellow),
            _ => t.dim(),
        };
        out.push((
            format!(
                "  {} · conversation {} · {state} · {range}",
                run_desc(d, run),
                truncate(conv, 10)
            ),
            style,
        ));
    }
    if suspended_binding(d).is_some() {
        out.push((
            "  Conversation changed — [c] Continue task   [n] Track new work".into(),
            t.bold(t.yellow),
        ));
    }
    // Messages.
    let msgs = arr(d, "messages");
    if !msgs.is_empty() {
        out.push((String::new(), t.text()));
        out.push(("Messages".into(), t.bold(t.accent)));
        for m in msgs.iter().rev().take(6) {
            let (s, style) = match st(m, "state") {
                "prepared" => ("prepared · not sent".to_string(), t.dim()),
                "sending" => ("sending…".to_string(), t.s(t.yellow)),
                "delivered" => ("✓ delivered".to_string(), t.s(t.green)),
                "delivery_unknown" => (
                    "? delivery unknown — check the pane before retrying".to_string(),
                    t.s(t.red),
                ),
                "failed" => (
                    format!(
                        "✗ failed{}",
                        m.get("detail")
                            .and_then(Value::as_str)
                            .map(|d| format!(": {d}"))
                            .unwrap_or_default()
                    ),
                    t.s(t.red),
                ),
                "cancelled" => ("cancelled".to_string(), t.dim()),
                other => (other.to_string(), t.dim()),
            };
            out.push((
                format!(
                    "  {s} · “{}”",
                    truncate(&st(m, "text").replace('\n', " "), 60)
                ),
                style,
            ));
        }
    }
    // Review package (T2).
    out.push((String::new(), t.text()));
    match &v.review {
        Api::Loading => out.push(("Review · loading…".into(), t.dim())),
        Api::Unsupported => out.push((
            "Review package unavailable on this server (needs T2)".into(),
            t.dim(),
        )),
        Api::Err(e) => out.push((format!("Review unavailable: {e}"), t.s(t.red))),
        Api::Ok(_) => {
            let p = package(v).cloned().unwrap_or(Value::Null);
            review_lines(app, &p, v, w, &mut out);
        }
    }
    out
}

fn review_lines(app: &App, p: &Value, v: &TaskView, w: usize, out: &mut Lines) {
    let t = app.theme;
    let subject = p.get("subject").cloned().unwrap_or(Value::Null);
    let label = st(p, "label");
    let acceptance = p.get("acceptance").filter(|a| !a.is_null());
    out.push((
        format!(
            "Review · {} · {} · package rev {}",
            if label.is_empty() {
                "Review available"
            } else {
                label_text(label)
            },
            subject_label(&subject),
            p.get("revision").and_then(Value::as_u64).unwrap_or(0)
        ),
        t.bold(t.accent),
    ));
    if !accept_capable(p) {
        out.push((
            "  Uncommitted work is inspectable; Select a committed revision to record acceptance"
                .into(),
            t.s(t.yellow),
        ));
        if p.pointer("/snapshot/available").and_then(Value::as_bool) == Some(true) {
            out.push((
                "  [s] Snapshot uncommitted work — an immutable, accept-capable candidate".into(),
                t.dim(),
            ));
        }
    }
    if let Some(a) = acceptance {
        acceptance_lines(app, p, a, w, out);
    }
    for b in arr(p, "blockers") {
        out.push((format!("  · {}", st(b, "message")), t.dim()));
    }
    let crit = criteria_of(p);
    if !crit.is_empty() {
        out.push(("Requirements".into(), t.bold(t.fg)));
    }
    for c in crit {
        let (tag, color) = match st(c, "status") {
            "supported" => ("PASS", t.green),
            "failed" => ("FAIL", t.red),
            "missing" => ("MISSING", t.yellow),
            "stale" => ("STALE", t.yellow),
            "needs_judgment" => ("NEEDS JUDGMENT", t.accent),
            _ => ("UNKNOWN", t.muted),
        };
        let req = if c.get("required").and_then(Value::as_bool).unwrap_or(true) {
            ""
        } else {
            " (optional)"
        };
        out.push((format!("  {tag:<15}{}{req}", st(c, "text")), t.bold(color)));
        let ev = c
            .get("evidence")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                arr(c, "reasons")
                    .first()
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        if let Some(e) = ev {
            wrap_push(out, &e, "                 ", w.saturating_sub(17), t.dim());
        }
    }
    let obs = {
        let a = arr(p, "observed");
        if a.is_empty() {
            arr(p, "observed_commands")
        } else {
            a
        }
    };
    if !obs.is_empty() {
        out.push(("Observed commands".into(), t.bold(t.fg)));
        for o in obs.iter().take(12) {
            if st(o, "category") == "claim" {
                out.push((
                    format!(
                        "  claim   “{}” — the agent's statement, not evidence",
                        truncate(st(o, "text"), 60)
                    ),
                    t.dim(),
                ));
                continue;
            }
            let code = o.get("exit_code").and_then(Value::as_i64);
            let mark = match code {
                Some(0) => "✓",
                Some(_) => "✗",
                None => "?",
            };
            let dur = o
                .get("duration_ms")
                .and_then(Value::as_i64)
                .map(|d| format!(", {:.1}s", d as f64 / 1000.0))
                .unwrap_or_default();
            let subj = st(o, "subject");
            let on = if subj.is_empty() {
                " · code binding unverified".to_string()
            } else {
                format!(" on {}", truncate(subj, 10))
            };
            out.push((
                format!(
                    "  {mark} {} (exit {}{dur}){on}",
                    truncate(st(o, "command"), 60),
                    code.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
                ),
                t.text(),
            ));
        }
    }
    let checks = defined_checks(v);
    if !checks.is_empty() {
        out.push(("Checks".into(), t.bold(t.fg)));
        for c in &checks {
            let auth = c.get("authorization").cloned().unwrap_or(Value::Null);
            let a = match st(&auth, "status") {
                "authorized" => "authorized for this revision".to_string(),
                "unavailable" => format!("unavailable: {}", st(&auth, "reason")),
                _ => "needs authorization for this revision".to_string(),
            };
            out.push((
                format!("  {} · {} · {a}", st(c, "name"), command_text(c)),
                t.text(),
            ));
        }
    }
    let runs = arr(p, "check_runs");
    if !runs.is_empty() {
        out.push(("Check runs".into(), t.bold(t.fg)));
        for r in runs.iter().take(8) {
            let state = st(r, "state");
            let color = match state {
                "passed" => t.green,
                "failed" => t.red,
                "running" | "queued" => t.yellow,
                _ => t.muted,
            };
            out.push((
                format!(
                    "  {state:<11}{} on {}{}",
                    st(r, "name"),
                    truncate(st(r, "subject"), 10),
                    r.get("summary")
                        .and_then(Value::as_str)
                        .map(|s| format!(" — {s}"))
                        .unwrap_or_default()
                ),
                t.s(color),
            ));
        }
    }
    crate::tasks_t4::review_lines(app, p, v, w, out);
    crate::tasks_2c::review_lines(app, p, w, out);
}

/// The acceptance block, from the server's shape `{acceptance: {subject_id, head_sha,
/// exceptions, ...}, label, status: current|outdated, outdated_reasons}` (15 §7). Green
/// **Reviewed on this subject** only for a current acceptance of the displayed subject with no
/// exceptions; exceptions are listed and never green; outdated is amber with its reasons.
fn acceptance_lines(app: &App, p: &Value, a: &Value, w: usize, out: &mut Lines) {
    let t = app.theme;
    let inner = a.get("acceptance").filter(|i| i.is_object()).unwrap_or(a);
    let displayed = subject_id_of(p);
    let accepted = st(inner, "subject_id");
    let head = st(inner, "head_sha");
    let short = &head[..head.len().min(8)];
    let exceptions = arr(inner, "exceptions");
    let status = st(a, "status");
    let same_subject = !accepted.is_empty() && accepted == displayed;
    let with_exc = if exceptions.is_empty() {
        String::new()
    } else {
        format!(" with exceptions ({})", exceptions.len())
    };
    let on = if short.is_empty() {
        format!("subject {}", truncate(accepted, 10))
    } else {
        format!("revision {short}")
    };
    let (line, color) = match status {
        "outdated" => (
            format!("  Review outdated — reviewed{with_exc} on {on}"),
            t.yellow,
        ),
        "current" if same_subject && exceptions.is_empty() => {
            ("  Reviewed on this subject".to_string(), t.green)
        }
        "current" if same_subject => (format!("  Reviewed{with_exc} on this subject"), t.yellow),
        "current" => (
            format!("  Reviewed{with_exc} on a different subject ({on}) — not the one shown"),
            t.muted,
        ),
        _ => (
            format!("  Reviewed{with_exc} on {on} — currency unknown"),
            t.muted,
        ),
    };
    out.push((line, t.bold(color)));
    if status == "outdated" {
        let reasons = arr(a, "outdated_reasons");
        if reasons.is_empty() {
            out.push(("    · reason not reported".into(), t.s(t.yellow)));
        }
        for r in reasons {
            let r = r
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| r.to_string());
            wrap_push(out, &r, "    · ", w.saturating_sub(6), t.s(t.yellow));
        }
    }
    let crit = criteria_of(p);
    for e in exceptions {
        let id = st(e, "criterion_id");
        let name = crit
            .iter()
            .find(|c| crit_id(c) == id)
            .map(|c| st(c, "text").to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.to_string());
        wrap_push(
            out,
            &format!("exception: {name} — {}", st(e, "reason")),
            "    ",
            w.saturating_sub(4),
            t.s(t.yellow),
        );
    }
}

fn command_text(c: &Value) -> String {
    match c.get("command") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        Some(Value::Object(o)) => o
            .get("shell")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                o.get("argv").and_then(Value::as_array).map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn sub_lines(app: &App, v: &TaskView, w: usize) -> Option<(String, Lines)> {
    let t = app.theme;
    let sel = |on: bool| if on { t.sel(t.accent) } else { t.text() };
    match &v.sub {
        TaskSub::None | TaskSub::T4(_) | TaskSub::Lane2c(_) => None,
        TaskSub::Edit(f) => {
            let mut l: Lines = Vec::new();
            l.push((
                format!(
                    "Editing intent (from revision {}) — draft, not saved",
                    f.base_revision
                ),
                t.dim(),
            ));
            l.push((
                format!("Title      {}", f.title),
                sel(f.field == IntentField::Title),
            ));
            l.push((
                format!("Objective  {}", f.objective),
                sel(f.field == IntentField::Objective),
            ));
            l.push((
                "Criteria   (ctrl+r required/optional, ctrl+d remove)".into(),
                t.dim(),
            ));
            for (i, c) in f.criteria.iter().enumerate() {
                l.push((
                    format!(
                        "  {} {}",
                        if c.required {
                            "[required]"
                        } else {
                            "[optional]"
                        },
                        c.text
                    ),
                    sel(f.field == IntentField::Criterion(i)),
                ));
            }
            l.push((
                "  + add criterion".into(),
                sel(f.field == IntentField::AddCriterion),
            ));
            l.push((
                format!("Stop at    ‹ {} ›", STOP_AT[f.stop].1),
                sel(f.field == IntentField::Stop),
            ));
            l.push((String::new(), t.text()));
            l.push(("[ Save details ]".into(), sel(f.field == IntentField::Save)));
            l.push((
                "[ Save and send clarification ]".into(),
                sel(f.field == IntentField::SaveSend),
            ));
            l.push((
                "Saving records a new revision and sends nothing; sending is a separate step"
                    .into(),
                t.dim(),
            ));
            if f.saving {
                l.push(("saving…".into(), t.s(t.yellow)));
            }
            if let Some(e) = &f.error {
                l.push((e.clone(), t.s(t.red)));
            }
            Some(("Edit task details".into(), l))
        }
        TaskSub::Exceptions(f) => {
            let mut l: Lines = Vec::new();
            l.push((format!("Mark reviewed · {}", f.subject_label), t.bold(t.fg)));
            l.push((
                format!(
                    "Records acceptance of intent revision {} and this exact subject. It does not merge, finish or grant anything.",
                    f.intent_revision
                ),
                t.dim(),
            ));
            if f.entries.is_empty() {
                l.push(("All required criteria are supported.".into(), t.s(t.green)));
            } else {
                l.push((
                    "These required criteria are not supported; enter an exception reason for each:"
                        .into(),
                    t.s(t.yellow),
                ));
            }
            for (i, e) in f.entries.iter().enumerate() {
                l.push((
                    format!("  {} — {}", e.status.to_uppercase(), e.text),
                    t.text(),
                ));
                l.push((format!("    reason: {}", e.reason), sel(f.field == i)));
            }
            let btn = if f.entries.is_empty() {
                "[ Mark reviewed ]"
            } else {
                "[ Mark reviewed with exceptions ]"
            };
            l.push((btn.into(), sel(f.field == f.entries.len())));
            if f.submitting {
                l.push(("recording…".into(), t.s(t.yellow)));
            }
            if let Some(e) = &f.error {
                l.push((e.clone(), t.s(t.red)));
            }
            Some(("Mark reviewed".into(), l))
        }
        TaskSub::Message(m) => {
            let mut l: Lines = Vec::new();
            let d = v.detail.clone().unwrap_or(Value::Null);
            let recipient = active_binding(&d)
                .map(|b| run_desc(&d, st(b, "run_id")))
                .unwrap_or_default();
            match &m.phase {
                MsgPhase::Compose | MsgPhase::Preparing => {
                    l.push((format!("Message to {recipient} (draft)"), t.bold(t.fg)));
                    wrap_push(&mut l, &format!("{}▏", m.text), "  ", w, t.text());
                    l.push((
                        "[enter] prepare (shows the exact text and recipient; sends nothing)  [alt+enter] newline  [esc] discard".into(),
                        t.dim(),
                    ));
                    if m.phase == MsgPhase::Preparing {
                        l.push(("preparing…".into(), t.s(t.yellow)));
                    }
                }
                MsgPhase::Prepared {
                    recipient,
                    unsafe_reason,
                    ..
                } => {
                    l.push((format!("To: {recipient}"), t.bold(t.fg)));
                    l.push(("Exact text:".into(), t.dim()));
                    wrap_push(&mut l, &m.text, "  │ ", w.saturating_sub(4), t.text());
                    if let Some(u) = unsafe_reason {
                        l.push((
                            format!("Sending from here is unsafe right now: {u}"),
                            t.s(t.yellow),
                        ));
                        l.push((
                            "[o] Open pane to send   [enter] try again (re-checked; zero bytes if unsafe)   [c] cancel".into(),
                            t.dim(),
                        ));
                    } else {
                        l.push((
                            "[enter] Send   [o] Open pane to send   [c] cancel".into(),
                            t.dim(),
                        ));
                    }
                }
                MsgPhase::Sending { message } => {
                    let state = message_state(&d, message)
                        .map(|(s, _)| s)
                        .unwrap_or_else(|| "sending".into());
                    l.push((format!("To: {recipient}"), t.bold(t.fg)));
                    wrap_push(&mut l, &m.text, "  │ ", w.saturating_sub(4), t.text());
                    l.push((
                        match state.as_str() {
                            "delivered" => "✓ delivered".to_string(),
                            "prepared" => "prepared · not sent yet".to_string(),
                            s => format!(
                                "{s}… (never shown as delivered until a matching turn starts)"
                            ),
                        },
                        t.s(t.yellow),
                    ));
                    l.push((
                        "[esc] close (delivery continues; see Messages)".into(),
                        t.dim(),
                    ));
                }
                MsgPhase::Refused { reason, .. } => {
                    l.push(("Not sent — zero bytes written".into(), t.bold(t.red)));
                    l.push((reason.clone(), t.s(t.red)));
                    wrap_push(&mut l, &m.text, "  │ ", w.saturating_sub(4), t.text());
                    l.push((
                        "[o] Open pane to send   [esc] close (draft kept in Messages)".into(),
                        t.dim(),
                    ));
                }
                MsgPhase::ConfirmRetry {
                    despite_unknown, ..
                } => {
                    l.push((
                        if *despite_unknown {
                            "Delivery unknown: the earlier message may have arrived.".to_string()
                        } else {
                            "The send failed.".to_string()
                        },
                        t.bold(t.yellow),
                    ));
                    if let Some(e) = &m.error {
                        l.push((e.clone(), t.s(t.red)));
                    }
                    l.push((
                        "Check the pane first. [y] Retry anyway (may send it twice)   [esc] don't retry".into(),
                        t.dim(),
                    ));
                }
            }
            if let Some(e) = &m.error
                && !matches!(m.phase, MsgPhase::ConfirmRetry { .. })
            {
                l.push((e.clone(), t.s(t.red)));
            }
            Some(("Message the agent".into(), l))
        }
        TaskSub::CheckPick { sel: s } => {
            let mut l: Lines = vec![("Choose a defined check".into(), t.bold(t.fg))];
            for (i, c) in defined_checks(v).iter().enumerate() {
                l.push((
                    format!("{} · {}", st(c, "name"), command_text(c)),
                    sel(i == *s),
                ));
            }
            Some(("Run check".into(), l))
        }
        TaskSub::Authorize(d) => {
            let check = &d.check;
            let auth = check.get("authorization").cloned().unwrap_or(Value::Null);
            let label = match st(&auth, "confirmation_label") {
                "" => "Runs code modified by this task",
                s => s,
            };
            let subject = &d.subject_label;
            let mut l: Lines = vec![
                (format!("Check: {}", st(check, "name")), t.bold(t.fg)),
                (format!("Command: {}", command_text(check)), t.text()),
                (
                    format!(
                        "Machine: {} · subject: {subject} · disposable checkout",
                        app.machines[v.machine].label
                    ),
                    t.text(),
                ),
                (format!("⚠ {label}"), t.bold(t.yellow)),
            ];
            for r in arr(&auth, "reasons") {
                let s = r
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| st(r, "kind").replace('_', " "));
                if !s.is_empty() {
                    l.push((format!("  · {s}"), t.dim()));
                }
            }
            if !d.digest.is_empty() {
                l.push((format!("Definition {}", truncate(&d.digest, 16)), t.dim()));
            }
            if d.stale {
                l.push((format!("✗ {CANDIDATE_CHANGED}"), t.bold(t.red)));
                l.push((
                    "Nothing will be authorized from this dialog   [esc] close".into(),
                    t.dim(),
                ));
            } else {
                l.push((
                    "[y] Authorize for this revision and run   [esc] cancel".into(),
                    t.dim(),
                ));
            }
            Some(("Authorize check".into(), l))
        }
    }
}

fn task_keys(v: &TaskView) -> String {
    let d = v.detail.clone().unwrap_or(Value::Null);
    let mut k = vec!["j/k scroll", "e edit intent", "w message agent"];
    if suspended_binding(&d).is_some() {
        k.push("c continue task");
        k.push("n track new work");
    }
    if !matches!(v.review, Api::Unsupported) {
        k.push("v run check");
        k.push("m mark reviewed");
    }
    k.extend(crate::tasks_t4::keys_hint(v));
    k.extend(crate::tasks_2c::keys_hint(v));
    k.extend([
        "S summarize review",
        "D drafts",
        "o open pane",
        "r refresh",
        "esc close",
    ]);
    k.join(" · ")
}

pub fn draw_task(app: &App, g: &mut Grid) {
    let t = app.theme;
    let Some(v) = &app.task_view else {
        return;
    };
    let area = app.pane_area();
    let r = SRect {
        x: area.x,
        y: area.y,
        w: area.w,
        h: area.h,
    };
    g.fill(r, t.text());
    let w = r.w.saturating_sub(4) as usize;
    let bottom = r.y + r.h.saturating_sub(1);
    let mut y = r.y;
    if let Some(n) = &v.notice {
        g.put_str(r.x + 1, y, n, t.bold(t.yellow), r.w.saturating_sub(2));
        y += 1;
    }
    if let TaskSub::T4(s) = &v.sub {
        crate::tasks_t4::draw_sub(app, g, v, s, r, y);
        return;
    }
    if let TaskSub::Lane2c(s) = &v.sub {
        crate::tasks_2c::draw_sub(app, g, s, r, y);
        return;
    }
    if let Some((title, lines)) = sub_lines(app, v, w) {
        g.put_str(r.x + 1, y, &title, t.bold(t.accent), r.w.saturating_sub(2));
        y += 1;
        for (s, stl) in lines {
            if y >= bottom {
                break;
            }
            g.put_str(r.x + 2, y, &s, stl, r.w.saturating_sub(3));
            y += 1;
        }
        g.put_str(
            r.x + 1,
            bottom,
            "tab/↑↓ move · type to edit · esc back",
            t.dim(),
            r.w.saturating_sub(2),
        );
        return;
    }
    let lines = task_lines(app, v, w);
    let rows = bottom.saturating_sub(y) as usize;
    let max_scroll = lines.len().saturating_sub(rows);
    let skip = (v.scroll as usize).min(max_scroll);
    for (s, stl) in lines.into_iter().skip(skip) {
        if y >= bottom {
            break;
        }
        g.put_str(r.x + 1, y, &s, stl, r.w.saturating_sub(2));
        y += 1;
    }
    g.put_str(
        r.x + 1,
        bottom,
        &task_keys(v),
        t.dim(),
        r.w.saturating_sub(2),
    );
}

pub fn draw_track(app: &App, g: &mut Grid) {
    let t = app.theme;
    let Some(f) = &app.track else {
        return;
    };
    let mut b = crate::popups::frame(app, g, 92, 30, "Track work in this session");
    let sel = |on: bool| if on { t.sel(t.accent) } else { t.text() };
    match f.phase {
        TrackPhase::Loading => {
            b.line("loading recent requests…", t.dim());
            return;
        }
        TrackPhase::Unverified => {
            b.line(
                "Link run first — this agent's session isn't verified",
                t.bold(t.yellow),
            );
            b.line(
                "Tracking needs a deterministic identity (installed integration with a reported session).",
                t.dim(),
            );
            if let Some(e) = &f.error {
                b.line(e, t.s(t.red));
            }
            let mut link = Vec::new();
            crate::tasks_2c::link_lines(app, f, &mut link);
            for (s, stl) in &link {
                b.line(s, *stl);
            }
            b.line("[esc] close", t.dim());
            return;
        }
        _ => {}
    }
    let w = b.width().saturating_sub(4) as usize;
    if f.turns.is_empty() {
        b.line(
            "No captured request in this session — type the title and criteria yourself",
            t.dim(),
        );
    } else {
        let turn = &f.turns[f.sel_turn];
        b.line(
            &format!(
                "From your selected request (turn {}, {}/{}; ↑/↓ another turn):",
                turn.n,
                f.sel_turn + 1,
                f.turns.len()
            ),
            sel(f.field == TrackField::Source),
        );
        let lines: Vec<&str> = turn.prompt.lines().collect();
        let mut shown = 0;
        for l in &lines {
            let mut out = Vec::new();
            wrap_push(&mut out, l, "  │ ", w.saturating_sub(4), t.text());
            for (s, stl) in out {
                if shown < 8 {
                    b.line(&s, stl);
                }
                shown += 1;
            }
        }
        if shown > 8 || turn.truncated {
            b.line("  │ …", t.dim());
        }
    }
    b.line("", t.text());
    b.line(
        &format!("Title      {}", f.title),
        sel(f.field == TrackField::Title),
    );
    if !f.constraints.is_empty() {
        b.line("Constraints (ctrl+d removes)", t.dim());
        for (i, c) in f.constraints.iter().enumerate() {
            b.line(
                &format!("  ◦ {}", c.text),
                sel(f.field == TrackField::Constraint(i)),
            );
        }
    }
    b.line(
        "Criteria   optional (enter adds another, ctrl+r required/optional, ctrl+d removes)",
        t.dim(),
    );
    for (i, c) in f.criteria.iter().enumerate() {
        let mut tags = vec![];
        if !c.required {
            tags.push("optional".to_string());
        }
        if let Some(e) = c.evaluation.as_deref().filter(|e| *e != "human") {
            tags.push(e.to_string());
        }
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!("  ({})", tags.join(", "))
        };
        b.line(
            &format!("  • {}{tags}", c.text),
            sel(f.field == TrackField::Criterion(i)),
        );
    }
    b.line(
        "  + add criterion",
        sel(f.field == TrackField::AddCriterion),
    );
    b.line(
        &format!("Stop at    ‹ {} ›", STOP_AT[f.stop].1),
        sel(f.field == TrackField::Stop),
    );
    b.line(
        &format!("Objective  {}", f.objective),
        sel(f.field == TrackField::Objective),
    );
    b.line("", t.text());
    let btn = match f.phase {
        TrackPhase::Submitting => "[ Tracking… ]",
        _ => "[ Track task ]",
    };
    b.line(btn, sel(f.field == TrackField::Submit));
    if let Some(e) = &f.error {
        b.line(e, t.s(t.red));
    }
    if f.assisted {
        b.line(
            "Suggested by the assistant — review and edit; nothing is saved until Track task.",
            t.s(t.accent),
        );
    }
    b.line(
        "Records the task and binds this run. Sends nothing, runs nothing, moves no files.",
        t.dim(),
    );
    b.line(
        "tab/↑↓ move · ←/→ stop point · ctrl+s track · ctrl+g Suggest task details · esc cancel",
        t.dim(),
    );
}

/// Tests: all grid rows as text.
#[cfg(test)]
pub fn grid_text(g: &Grid) -> String {
    let mut s = String::new();
    let mut y = 0;
    while g.get(0, y).is_some() {
        for c in g.row(y) {
            s.push_str(c.text.as_str());
        }
        s.push('\n');
        y += 1;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_app;
    use vk_proto::input::Mods;
    use vk_proto::render::{ClientFrame, ServerFrame};

    fn key(k: Key) -> KeyEvent {
        KeyEvent::new(k, Mods::empty())
    }
    fn ch(c: char) -> KeyEvent {
        key(Key::Char(c))
    }
    fn typ(f: &mut TrackForm, s: &str) {
        for c in s.chars() {
            f.key(&ch(c));
        }
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
    fn reply_err(app: &mut App, mi: usize, req: u64, kind: &str, details: Value) {
        let json = json!({"jsonrpc": "2.0", "id": req, "error": {"code": -32000, "message": format!("{kind}!"), "data": {"kind": kind, "details": details}}}).to_string();
        app.on_frame(mi, ServerFrame::CommandResult { req, json });
    }

    fn sources() -> Value {
        json!({"run": "r1", "harness": "claude", "identity_verified": true,
        "turns": [
          {"n": 1, "prompt": "Old request\nwith detail", "prompt_truncated": false},
          {"n": 2, "prompt": "Fix the login redirect.\nPreserve SSO. Open a draft PR when ready.", "prompt_truncated": false}
        ]})
    }

    #[test]
    fn track_form_state_machine() {
        let mut f = TrackForm::new(1, 0, "r1", "p1", "key-1".into());
        assert_eq!(f.key(&ch('x')), FormOutcome::Stay);
        f.load_sources(&sources());
        // Latest turn preselected; title is its first line; stop starts Not specified.
        assert_eq!(f.turns[f.sel_turn].n, 2);
        assert_eq!(f.title, "Fix the login redirect.");
        assert_eq!(STOP_AT[f.stop].1, "Not specified");
        // ↓ picks the older turn and retitles while the title is untouched.
        f.key(&key(Key::Named(NamedKey::Down)));
        assert_eq!(f.turns[f.sel_turn].n, 1);
        assert_eq!(f.title, "Old request");
        f.key(&key(Key::Named(NamedKey::Up)));
        assert_eq!(f.title, "Fix the login redirect.");
        // Edit the title: later turn changes no longer overwrite it.
        f.key(&key(Key::Named(NamedKey::Tab)));
        assert_eq!(f.field, TrackField::Title);
        typ(&mut f, " now");
        assert!(f.title_edited);
        // Add two criteria, remove one with backspace on empty.
        f.key(&key(Key::Named(NamedKey::Tab)));
        assert_eq!(f.field, TrackField::AddCriterion);
        f.key(&key(Key::Named(NamedKey::Enter)));
        typ(&mut f, "Preserve SSO");
        f.key(&key(Key::Named(NamedKey::Enter)));
        assert_eq!(f.field, TrackField::Criterion(1));
        f.key(&key(Key::Named(NamedKey::Backspace)));
        assert_eq!(f.criteria, vec![TrackItem::typed("Preserve SSO")]);
        assert_eq!(f.field, TrackField::Criterion(0));
        // Stop-at chooser cycles; objective; submit.
        f.key(&key(Key::Named(NamedKey::Tab)));
        f.key(&key(Key::Named(NamedKey::Tab)));
        assert_eq!(f.field, TrackField::Stop);
        f.key(&key(Key::Named(NamedKey::Right)));
        f.key(&key(Key::Named(NamedKey::Right)));
        assert_eq!(STOP_AT[f.stop].0, "draft_pr");
        f.key(&key(Key::Named(NamedKey::Left)));
        f.key(&key(Key::Named(NamedKey::Left)));
        f.key(&key(Key::Named(NamedKey::Left)));
        assert_eq!(STOP_AT[f.stop].0, "no_separate_outcome");
        f.key(&key(Key::Named(NamedKey::Enter)));
        assert_eq!(f.field, TrackField::Objective);
        typ(&mut f, "Return users to their page");
        f.key(&key(Key::Named(NamedKey::Tab)));
        assert_eq!(
            f.key(&key(Key::Named(NamedKey::Enter))),
            FormOutcome::Submit
        );
        let p = f.params();
        assert_eq!(p["turn"], 2);
        assert_eq!(p["title"], "Fix the login redirect. now");
        assert_eq!(p["criteria"], json!(["Preserve SSO"]));
        assert_eq!(p["stop_at"], "no_separate_outcome");
        assert_eq!(p["idempotency_key"], "key-1");
        // An empty title can't be submitted.
        f.title.clear();
        assert_eq!(
            f.key(&KeyEvent::new(Key::Char('s'), Mods::CTRL)),
            FormOutcome::Stay
        );
        assert_eq!(
            f.key(&key(Key::Named(NamedKey::Escape))),
            FormOutcome::Cancel
        );
    }

    #[test]
    fn unverified_identity_offers_no_track() {
        let mut f = TrackForm::new(1, 0, "r1", "p1", "k".into());
        let mut s = sources();
        s["identity_verified"] = json!(false);
        f.load_sources(&s);
        assert_eq!(f.phase, TrackPhase::Unverified);
        assert_eq!(
            f.key(&KeyEvent::new(Key::Char('s'), Mods::CTRL)),
            FormOutcome::Stay
        );
        assert_eq!(f.key(&key(Key::Named(NamedKey::Enter))), FormOutcome::Stay);
    }

    fn app_with_run() -> (App, Vec<tokio::sync::mpsc::UnboundedReceiver<ClientFrame>>) {
        let (mut app, rxs) = test_app(1);
        app.machines[0]
            .model
            .runs
            .push(crate::app::test_run("r1", "p1", "claude"));
        (app, rxs)
    }

    #[test]
    fn track_flow_persists_key_and_reuses_it_after_a_refusal() {
        let (mut app, mut rxs) = app_with_run();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client-pending-c.json");
        app.pending_ops = crate::pending::PendingStore::open(dir.path().to_path_buf(), "c");
        open_track(&mut app, 0, "p1");
        let c = commands(&mut rxs[0]);
        assert_eq!(c[0].1["method"], "task.sources");
        reply(&mut app, 0, c[0].0, sources());
        assert_eq!(app.track.as_ref().unwrap().phase, TrackPhase::Ready);
        let key0 = app.track.as_ref().unwrap().idem.clone();
        app.on_key(KeyEvent::new(Key::Char('s'), Mods::CTRL));
        // Persisted before dispatch.
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains(&key0) && saved.contains("task.track"));
        let c = commands(&mut rxs[0]);
        assert_eq!(c[0].1["method"], "task.track");
        assert_eq!(c[0].1["params"]["idempotency_key"], key0.as_str());
        // A definitive refusal clears the pending op and keeps the form (same key for retry).
        reply_err(
            &mut app,
            0,
            c[0].0,
            "conflict",
            json!({"reason": "binding_conflict"}),
        );
        assert!(app.pending_ops.ops.is_empty());
        let f = app.track.as_ref().unwrap();
        assert_eq!(f.phase, TrackPhase::Ready);
        assert!(f.error.is_some());
        assert_eq!(f.idem, key0);
        // Retry → success: the form closes, the run is now known to be tracked.
        app.on_key(KeyEvent::new(Key::Char('s'), Mods::CTRL));
        let c = commands(&mut rxs[0]);
        assert_eq!(c[0].1["params"]["idempotency_key"], key0.as_str());
        reply(
            &mut app,
            0,
            c[0].0,
            json!({"task": {"id": "t1", "handle": "1", "title": "Fix the login redirect."},
                   "binding": {"run_id": "r1", "state": "active"}}),
        );
        assert!(app.track.is_none());
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(
            app.task_runs
                .get(&(0, "r1".to_string()))
                .map(String::as_str),
            Some("t1")
        );
        assert!(app.pending_ops.ops.is_empty());
    }

    #[test]
    fn binding_unverified_from_server_shows_link_first() {
        let (mut app, mut rxs) = app_with_run();
        open_track(&mut app, 0, "p1");
        let c = commands(&mut rxs[0]);
        reply_err(
            &mut app,
            0,
            c[0].0,
            "conflict",
            json!({"reason": "binding_unverified"}),
        );
        assert_eq!(app.track.as_ref().unwrap().phase, TrackPhase::Unverified);
        app.size = (120, 40);
        let mut g = Grid::new(120, 40);
        crate::draw::compose(&app, &mut g);
        let text = grid_text(&g);
        assert!(text.contains("Link run first — this agent's session isn't verified"));
        assert!(!text.contains("[ Track task ]"));
    }

    fn package(dirty: bool) -> Value {
        json!({"package": {
            "revision": 4, "intent_revision": 2, "label": "review_available",
            "subject": {"id": "subj-1", "kind": if dirty {"dirty"} else {"committed"}, "head_sha": "abc12345def", "accept_capable": !dirty},
            "criteria": [
                {"criterion_id": "c1", "version": 1, "text": "Return to original page", "required": true, "evaluation": "check", "status": "supported", "evidence": "Vibeke verification passed on commit abc123"},
                {"criterion_id": "c2", "version": 1, "text": "Preserve SSO", "required": true, "evaluation": "check", "status": "missing", "evidence": "Agent claims compatibility; no matching SSO check"},
                {"criterion_id": "c3", "version": 2, "text": "Deliver a draft PR", "required": true, "evaluation": "external", "status": "stale"},
                {"criterion_id": "c4", "version": 1, "text": "Nice to have", "required": false, "evaluation": "human", "status": "missing"}
            ],
            "observed": [
                {"command": "cargo test", "exit_code": 0, "duration_ms": 3200, "subject": "abc12345"},
                {"category": "claim", "text": "all tests pass"}
            ],
            "checks": [{"id": "ck1", "name": "sso-smoke", "command": {"shell": "npm run sso"}, "authorization": {"status": "required", "confirmation_label": "Runs code modified by this task"}}],
            "check_runs": [{"id": "cr1", "name": "unit", "state": "failed", "subject": "abc12345"}]
        }})
    }

    #[test]
    fn exception_entry_requires_a_reason_per_missing_required_criterion() {
        let p = package(false);
        let mut f = ExceptionForm::from_package(&p["package"], "acc-1".into());
        let ids: Vec<&str> = f.entries.iter().map(|e| e.criterion_id.as_str()).collect();
        assert_eq!(ids, vec!["c2", "c3"]);
        // Submit is refused until every entry has a reason.
        f.field = 2;
        assert_eq!(f.key(&key(Key::Named(NamedKey::Enter))), FormOutcome::Stay);
        assert!(f.error.is_some());
        f.field = 0;
        for c in "covered manually".chars() {
            f.key(&ch(c));
        }
        f.key(&key(Key::Named(NamedKey::Enter)));
        assert_eq!(f.field, 1);
        f.key(&key(Key::Named(NamedKey::Down)));
        assert_eq!(f.key(&key(Key::Named(NamedKey::Enter))), FormOutcome::Stay);
        f.key(&key(Key::Named(NamedKey::Up)));
        for c in "PR opened by hand".chars() {
            f.key(&ch(c));
        }
        f.key(&key(Key::Named(NamedKey::Tab)));
        assert_eq!(
            f.key(&key(Key::Named(NamedKey::Enter))),
            FormOutcome::Submit
        );
        let params = f.params("t1");
        assert_eq!(params["expected_intent_revision"], 2);
        assert_eq!(params["expected_package_revision"], 4);
        assert_eq!(params["subject_id"], "subj-1");
        assert_eq!(params["exceptions"][0]["reason"], "covered manually");
        assert_eq!(params["exceptions"][1]["criterion_id"], "c3");
        assert_eq!(params["exceptions"][1]["version"], 2);
    }

    fn detail() -> Value {
        json!({
            "task": {"id": "t1", "handle": "7", "title": "Fix login redirect", "ownership": "attached", "status": "active", "review_label": "review_available", "effort": "quick"},
            "intent": {"revision": 2, "title": "Fix login redirect", "objective": "Return users to their original page",
                       "criteria": [{"id": "c1", "version": 1, "text": "Preserve SSO", "required": true, "evaluation": "check", "check_definition_ids": ["ck1"]}],
                       "constraints": [], "stop_at": "draft_pr",
                       "source_excerpt": "Fix the login redirect. Preserve SSO. Open a draft PR when ready."},
            "uncommunicated": ["c1"],
            "bindings": [
                {"id": "b1", "run_id": "r1", "native_conversation_id": "sess-aaaa", "state": "suspended", "start_turn": 1, "end_turn": 4}
            ],
            "runs": [{"id": "r1", "handle": "3", "harness": "claude", "pane": "p1", "execution": "Idle", "ended": false}],
            "messages": [
                {"id": "m1", "text": "Please add the test", "state": "delivery_unknown", "run": "r1"},
                {"id": "m2", "text": "Second", "state": "sending", "run": "r1"}
            ]
        })
    }

    fn open_view(
        app: &mut App,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientFrame>,
        review: Option<Value>,
    ) {
        open_task(app, 0, "t1");
        let c = commands(rx);
        for (req, v) in c {
            match v["method"].as_str().unwrap() {
                "task.detail" => reply(app, 0, req, detail()),
                "task.review.get" => match &review {
                    Some(p) => reply(app, 0, req, p.clone()),
                    None => {
                        let json = json!({"jsonrpc":"2.0","id":req,"error":{"code":-32601,"message":"no","data":{"kind":"method_not_found"}}}).to_string();
                        app.on_frame(0, ServerFrame::CommandResult { req, json });
                    }
                },
                "task.check.list" => {
                    let json = json!({"jsonrpc":"2.0","id":req,"error":{"code":-32601,"message":"no","data":{"kind":"method_not_found"}}}).to_string();
                    app.on_frame(0, ServerFrame::CommandResult { req, json });
                }
                m => panic!("unexpected {m}"),
            }
        }
    }

    #[test]
    fn task_detail_draws_intent_bindings_messages_and_review() {
        let (mut app, mut rxs) = app_with_run();
        app.size = (140, 70);
        open_view(&mut app, &mut rxs[0], Some(package(false)));
        let mut g = Grid::new(140, 70);
        crate::draw::compose(&app, &mut g);
        let text = grid_text(&g);
        for want in [
            "Fix login redirect",
            "Review available",
            "#7 · attached · active",
            "Agent has not been told about revision 2",
            "Preserve SSO  [required · check]",
            "Stop at: Draft PR",
            "Fix the login redirect. Preserve SSO.",
            "Continue task",
            "Track new work",
            "delivery unknown",
            "sending…",
            "PASS",
            "MISSING",
            "STALE",
            "Vibeke verification passed on commit abc123",
            "cargo test (exit 0, 3.2s) on abc12345",
            "the agent's statement, not evidence",
            "sso-smoke",
            "needs authorization",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
        // No delivery state is ever rendered as delivered unless it is.
        assert!(!text.contains("✓ delivered"));
        // A sending message is being polled.
        assert!(app.task_view.as_ref().unwrap().watch.contains("m2"));
    }

    #[test]
    fn mark_reviewed_on_dirty_subject_explains_and_does_not_open_form() {
        let (mut app, mut rxs) = app_with_run();
        open_view(&mut app, &mut rxs[0], Some(package(true)));
        app.on_key(ch('m'));
        let v = app.task_view.as_ref().unwrap();
        assert_eq!(v.sub, TaskSub::None);
        assert_eq!(
            v.notice.as_deref(),
            Some("Select a committed revision to record acceptance")
        );
    }

    #[test]
    fn run_check_needs_authorization_first() {
        let (mut app, mut rxs) = app_with_run();
        open_view(&mut app, &mut rxs[0], Some(package(false)));
        app.on_key(ch('v'));
        assert!(matches!(
            app.task_view.as_ref().unwrap().sub,
            TaskSub::Authorize(_)
        ));
        assert!(commands(&mut rxs[0]).is_empty());
        app.on_key(ch('y'));
        let c = commands(&mut rxs[0]);
        assert_eq!(c[0].1["method"], "task.check.run");
        assert_eq!(c[0].1["params"]["authorize"], true);
        assert_eq!(c[0].1["params"]["subject"], "subj-1");
        assert_eq!(c[0].1["params"]["check"], "ck1");
    }

    #[test]
    fn t1_only_server_hides_review_actions_and_continue_binds() {
        let (mut app, mut rxs) = app_with_run();
        open_view(&mut app, &mut rxs[0], None);
        assert_eq!(app.task_view.as_ref().unwrap().review, Api::Unsupported);
        app.size = (140, 60);
        let mut g = Grid::new(140, 60);
        crate::draw::compose(&app, &mut g);
        let text = grid_text(&g);
        assert!(text.contains("needs T2"), "{text}");
        assert!(!text.contains("m mark reviewed"));
        app.on_key(ch('c'));
        let c = commands(&mut rxs[0]);
        assert_eq!(c[0].1["method"], "task.bind");
        assert_eq!(c[0].1["params"]["run"], "r1");
        assert!(c[0].1["params"]["idempotency_key"].is_string());
    }

    #[test]
    fn edit_intent_save_and_send_flow_with_refusal() {
        let (mut app, mut rxs) = app_with_run();
        open_view(&mut app, &mut rxs[0], None);
        // Make the binding active so messaging is possible.
        if let Some(d) = &mut app.task_view.as_mut().unwrap().detail {
            d["bindings"][0]["state"] = json!("active");
        }
        app.on_key(ch('e'));
        assert!(matches!(
            app.task_view.as_ref().unwrap().sub,
            TaskSub::Edit(_)
        ));
        for c in " now".chars() {
            app.on_key(ch(c));
        }
        // Jump to "Save and send clarification".
        for _ in 0..10 {
            app.on_key(key(Key::Named(NamedKey::Down)));
        }
        app.on_key(key(Key::Named(NamedKey::Enter)));
        let c = commands(&mut rxs[0]);
        assert_eq!(c[0].1["method"], "task.intent.update");
        assert_eq!(c[0].1["params"]["expected_revision"], 2);
        assert_eq!(c[0].1["params"]["title"], "Fix login redirect now");
        assert_eq!(c[0].1["params"]["criteria"][0]["checks"], json!(["ck1"]));
        // Saved: nothing was sent; a draft with the exact text opens.
        reply(
            &mut app,
            0,
            c[0].0,
            json!({"intent": {"revision": 3, "title": "Fix login redirect now", "objective": "",
                              "criteria": [{"text": "Preserve SSO", "required": true}], "stop_at": "draft_pr"},
                   "note": "Agent has not been told about revision 3"}),
        );
        let v = app.task_view.as_ref().unwrap();
        let TaskSub::Message(m) = &v.sub else {
            panic!("{:?}", v.sub)
        };
        assert!(m.text.contains("revision 3") && m.text.contains("- Preserve SSO (required)"));
        assert!(m.communicates_intent);
        let c = commands(&mut rxs[0]);
        assert!(c.iter().all(|(_, v)| v["method"] != "task.message.send"));
        // Prepare → exact text + recipient, then an explicit Send.
        app.on_key(key(Key::Named(NamedKey::Enter)));
        let c = commands(&mut rxs[0]);
        let prep = c
            .iter()
            .find(|(_, v)| v["method"] == "task.message.prepare")
            .unwrap();
        assert_eq!(prep.1["params"]["communicates_intent"], true);
        reply(
            &mut app,
            0,
            prep.0,
            json!({"message": {"id": "m9", "text": "exact text", "run": "r1"}, "recipient": {"run": "r1"}, "unsafe": null}),
        );
        let TaskSub::Message(m) = &app.task_view.as_ref().unwrap().sub else {
            panic!()
        };
        assert!(
            matches!(&m.phase, MsgPhase::Prepared { recipient, .. } if recipient == "claude #3")
        );
        app.on_key(key(Key::Named(NamedKey::Enter)));
        let c = commands(&mut rxs[0]);
        let send = c
            .iter()
            .find(|(_, v)| v["method"] == "task.message.send")
            .unwrap();
        reply_err(
            &mut app,
            0,
            send.0,
            "conflict",
            json!({"reason": "send_unsafe", "detail": "an attached client is focused on that pane", "pane": "p1"}),
        );
        let TaskSub::Message(m) = &app.task_view.as_ref().unwrap().sub else {
            panic!()
        };
        assert!(matches!(&m.phase, MsgPhase::Refused { reason, .. } if reason.contains("focused")));
        // Only the explicit Open pane action focuses the pane.
        assert_ne!(app.focused_pane().as_deref(), Some("p1"));
        app.on_key(ch('o'));
        assert!(matches!(app.mode, Mode::Normal));
    }

    fn package_with(subject: &str, digest: &str) -> Value {
        let mut p = package(false);
        p["package"]["subject"]["id"] = json!(subject);
        p["package"]["checks"][0]["definition"] = json!({"id": "ck1", "definition_digest": digest});
        p
    }

    /// Answer the refresh `fetch` sends (detail + review; check.list is unsupported here).
    fn answer_refresh(
        app: &mut App,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClientFrame>,
        review: Value,
    ) {
        for (req, v) in commands(rx) {
            match v["method"].as_str().unwrap() {
                "task.detail" => reply(app, 0, req, detail()),
                "task.review.get" => reply(app, 0, req, review.clone()),
                m => panic!("unexpected {m}"),
            }
        }
    }

    fn auth_dialog(app: &App) -> AuthDialog {
        match &app.task_view.as_ref().unwrap().sub {
            TaskSub::Authorize(d) => d.clone(),
            s => panic!("not authorizing: {s:?}"),
        }
    }

    /// Codex G02 #1: the dialog freezes the subject and definition digest it shows; a refresh
    /// that changes either invalidates it, and confirming sends exactly what was shown.
    #[test]
    fn authorization_dialog_is_frozen_and_invalidated_by_a_changed_candidate() {
        // Unchanged refresh: confirm sends the frozen subject + digest.
        let (mut app, mut rxs) = app_with_run();
        open_view(&mut app, &mut rxs[0], Some(package_with("subj-A", "dig-1")));
        app.on_key(ch('v'));
        let d = auth_dialog(&app);
        assert_eq!((d.subject.as_str(), d.digest.as_str()), ("subj-A", "dig-1"));
        assert!(!d.stale);
        fetch(&mut app, false);
        answer_refresh(&mut app, &mut rxs[0], package_with("subj-A", "dig-1"));
        assert!(!auth_dialog(&app).stale);
        app.on_key(ch('y'));
        let c = commands(&mut rxs[0]);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].1["method"], "task.check.run");
        assert_eq!(c[0].1["params"]["authorize"], true);
        assert_eq!(c[0].1["params"]["subject"], "subj-A");
        assert_eq!(c[0].1["params"]["definition_digest"], "dig-1");

        // Candidate B installed while the dialog is open: invalidated, nothing authorized.
        for changed in [
            package_with("subj-B", "dig-1"),
            package_with("subj-A", "dig-2"),
        ] {
            let (mut app, mut rxs) = app_with_run();
            app.size = (140, 50);
            open_view(&mut app, &mut rxs[0], Some(package_with("subj-A", "dig-1")));
            app.on_key(ch('v'));
            fetch(&mut app, false);
            answer_refresh(&mut app, &mut rxs[0], changed);
            let d = auth_dialog(&app);
            assert!(d.stale);
            // The dialog still names what it showed, never the new candidate.
            assert_eq!(d.subject, "subj-A");
            assert_eq!(d.digest, "dig-1");
            let mut g = Grid::new(140, 50);
            crate::draw::compose(&app, &mut g);
            let text = grid_text(&g);
            assert!(text.contains(CANDIDATE_CHANGED), "{text}");
            assert!(!text.contains("[y] Authorize"), "{text}");
            app.on_key(ch('y'));
            assert!(commands(&mut rxs[0]).is_empty());
            assert!(auth_dialog(&app).stale);
            app.on_key(key(Key::Named(NamedKey::Escape)));
            assert_eq!(app.task_view.as_ref().unwrap().sub, TaskSub::None);
            assert!(commands(&mut rxs[0]).is_empty());
        }
    }

    fn with_acceptance(acc: Value) -> Value {
        let mut p = package(false);
        p["package"]["acceptance"] = acc;
        p
    }

    fn acceptance_block(acc: Value) -> (Vec<(String, Style)>, crate::theme::Theme) {
        let (mut app, mut rxs) = app_with_run();
        open_view(&mut app, &mut rxs[0], Some(with_acceptance(acc)));
        let v = app.task_view.as_ref().unwrap();
        (task_lines(&app, v, 120), app.theme)
    }

    fn find<'a>(lines: &'a [(String, Style)], s: &str) -> Option<&'a (String, Style)> {
        lines.iter().find(|(l, _)| l.contains(s))
    }

    /// Codex G02 #11: acceptance rendered from the server's real shape.
    #[test]
    fn acceptance_renders_current_excepted_outdated_and_other_subject_honestly() {
        let inner = |subject: &str, exceptions: Value| {
            json!({"id": "acc1", "subject_id": subject, "head_sha": "0ld5ha99aa", "intent_revision": 2,
                   "package_revision": 4, "exceptions": exceptions})
        };
        let exc = json!([{"criterion_id": "c2", "reason": "SSO verified manually on staging"}]);

        // Current, this subject, no exceptions: the only green case.
        let (l, t) = acceptance_block(json!({"acceptance": inner("subj-1", json!([])),
            "label": "Reviewed", "status": "current", "outdated_reasons": []}));
        let line = find(&l, "Reviewed on this subject").expect("reviewed line");
        assert_eq!(line.1, t.bold(t.green));

        // Current, this subject, with exceptions: listed, never green.
        let (l, t) = acceptance_block(json!({"acceptance": inner("subj-1", exc.clone()),
            "label": "Reviewed with exceptions", "status": "current", "outdated_reasons": []}));
        assert!(find(&l, "Reviewed on this subject").is_none());
        let line = find(&l, "Reviewed with exceptions (1) on this subject").expect("excepted");
        assert_ne!(line.1, t.bold(t.green));
        let e = find(
            &l,
            "exception: Preserve SSO — SSO verified manually on staging",
        )
        .expect("exception listed");
        assert_eq!(e.1, t.s(t.yellow));

        // Outdated: amber with its reasons, even on the same subject.
        let (l, t) = acceptance_block(json!({"acceptance": inner("subj-1", exc.clone()),
            "label": "Reviewed with exceptions", "status": "outdated",
            "outdated_reasons": ["check definition changed: sso-smoke"]}));
        assert!(find(&l, "Reviewed on this subject").is_none());
        let line = find(
            &l,
            "Review outdated — reviewed with exceptions (1) on revision 0ld5ha99",
        )
        .expect("outdated line");
        assert_eq!(line.1, t.bold(t.yellow));
        assert!(find(&l, "check definition changed: sso-smoke").is_some());
        assert!(find(&l, "exception: Preserve SSO").is_some());

        // Current, but accepted on another subject (A) while B is displayed.
        let (l, t) = acceptance_block(json!({"acceptance": inner("subj-A", exc),
            "label": "Reviewed with exceptions", "status": "current", "outdated_reasons": []}));
        assert!(find(&l, "Reviewed on this subject").is_none());
        assert!(find(&l, "on this subject").is_none());
        let line = find(&l, "on a different subject (revision 0ld5ha99)").expect("other subject");
        assert_ne!(line.1, t.bold(t.green));
        assert!(find(&l, "exception: Preserve SSO").is_some());
    }
}
