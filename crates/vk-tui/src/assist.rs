//! Assist actions (14; server: 07 §2.15b `assistant.*`). Every action is user-invoked and
//! runs the same flow:
//!
//! 1. `assistant.generate` → the **exact preview** (system + user text, model, endpoint host,
//!    execution machine, bytes, token/cost estimates, redactions, omissions) is shown;
//! 2. nothing leaves the machine until the user confirms (`y`): `assistant.confirm` with the
//!    preview digest; `n`/`esc` cancels the prepared request (`assistant.cancel`);
//! 3. the result (`assistant.get`, polled while waiting and refreshed on pushed
//!    `assistant.request_*` events) becomes an **editable draft only**: suggested task details
//!    fill the Track form (still unsaved), a review summary / briefing is editable text that can
//!    be saved as a draft (`ctrl+d`), a suggested pane title is applied only with `enter`.
//!
//! Disabled and consent states are explicit: "Assistance is off — enable in config", and
//! "Grant consent for this workspace?" with `g` calling `assistant.consent` (never implicit).
//!
//! Entry points: Track form `ctrl+g` **Suggest task details**, task details `S` **Summarize
//! review**, palette `assist_pane_title` / agent peek `s` **Suggest title**, palette
//! `assist_briefing` **Briefing**.

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::drafts::{Area, TextEditor, arr, ctrl, st};
use crate::screen::Grid;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};

pub const OFF: &str = "Assistance is off — enable in config";
pub const CONSENT: &str = "Grant consent for this workspace?";
const POLL: Duration = Duration::from_millis(1000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    SuggestTaskDetails,
    ReviewSummary,
    PaneTitle,
    Briefing,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::SuggestTaskDetails => "suggest_task_details",
            Op::ReviewSummary => "review_summary",
            Op::PaneTitle => "pane_title",
            Op::Briefing => "briefing",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Op::SuggestTaskDetails => "Suggest task details",
            Op::ReviewSummary => "Summarize review",
            Op::PaneTitle => "Suggest title",
            Op::Briefing => "Briefing",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    Palette,
    /// The Track form (its run).
    Track,
    Task,
    Peek(String),
}

#[derive(Debug, Clone)]
pub enum Reply {
    Generate { flow: u64 },
    Consent { flow: u64 },
    Confirm { flow: u64 },
    Get { flow: u64 },
    Cancel,
    Applied { flow: u64 },
    DraftSaved { flow: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Phase {
    Generating,
    Disabled(String),
    NeedsConsent {
        reason: String,
    },
    Granting,
    Preview {
        request: String,
        preview: Value,
        scroll: u16,
    },
    Confirming {
        request: String,
    },
    Waiting {
        request: String,
        /// The server sent without asking (`auto_send` in config and consent).
        auto: bool,
    },
    Done {
        request: String,
        output: Value,
        ed: TextEditor,
    },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Flow {
    pub id: u64,
    pub machine: usize,
    pub op: Op,
    pub inputs: Value,
    pub origin: Origin,
    pub workspace: Option<String>,
    pub phase: Phase,
    pub idem: String,
    pub notice: Option<String>,
    pub last_poll: Option<Instant>,
}

// ---- starting -------------------------------------------------------------------------------------

fn ws_of_pane(app: &App, mi: usize, pane: &str) -> Option<String> {
    app.machines[mi]
        .model
        .panes
        .iter()
        .find(|p| p.id == pane)
        .map(|p| p.workspace.clone())
}

pub fn start(app: &mut App, mi: usize, op: Op, inputs: Value, origin: Origin, ws: Option<String>) {
    let id = app.next_ui_id();
    let idem = app.new_idempotency_key("assist");
    let f = Flow {
        id,
        machine: mi,
        op,
        inputs,
        origin,
        workspace: ws,
        phase: Phase::Generating,
        idem,
        notice: None,
        last_poll: None,
    };
    generate(app, &f);
    app.assist = Some(f);
    app.mode = Mode::Popup(Popup::Assist);
}

fn generate(app: &mut App, f: &Flow) {
    let mut p = json!({"operation": f.op.as_str(), "idempotency_key": f.idem});
    if let (Some(o), Some(i)) = (p.as_object_mut(), f.inputs.as_object()) {
        for (k, v) in i {
            o.insert(k.clone(), v.clone());
        }
    }
    app.command_on(
        f.machine,
        "assistant.generate",
        p,
        Pending::Assist(Reply::Generate { flow: f.id }),
    );
}

/// Track form `ctrl+g`: suggest task details from the selected request only.
pub fn suggest_task_details(app: &mut App) {
    let Some(f) = &app.track else {
        return;
    };
    let mi = f.machine;
    let mut inputs = json!({"run": f.run});
    if let Some(t) = f.turns.get(f.sel_turn) {
        inputs["turns"] = json!([t.n]);
    }
    let ws = ws_of_pane(app, mi, &f.pane.clone());
    start(app, mi, Op::SuggestTaskDetails, inputs, Origin::Track, ws);
}

/// Task details `S`.
pub fn summarize_review(app: &mut App, mi: usize, task: &str) {
    let ws = app.machines[mi]
        .model
        .tasks
        .iter()
        .find(|t| t.id == task)
        .and_then(|t| t.workspace.clone())
        .or_else(|| app.focused_ws().map(|w| w.id));
    start(
        app,
        mi,
        Op::ReviewSummary,
        json!({"task": task}),
        Origin::Task,
        ws,
    );
}

/// Peek `s` / palette `assist_pane_title`.
pub fn suggest_title(app: &mut App, mi: usize, pane: &str, origin: Origin) {
    let ws = ws_of_pane(app, mi, pane);
    start(app, mi, Op::PaneTitle, json!({"pane": pane}), origin, ws);
}

/// Palette actions owned by this module.
pub fn action(app: &mut App, action: &str) -> bool {
    match action {
        "assist_briefing" | "briefing" => {
            let mi = app.cur;
            match app.focused_ws() {
                Some(w) => start(
                    app,
                    mi,
                    Op::Briefing,
                    json!({"workspace": w.id}),
                    Origin::Palette,
                    Some(w.id),
                ),
                None => app.toast("no workspace"),
            }
            true
        }
        "assist_pane_title" => {
            let mi = app.cur;
            match app.focused_pane() {
                Some(p) => suggest_title(app, mi, &p, Origin::Palette),
                None => app.toast("no focused pane"),
            }
            true
        }
        _ => false,
    }
}

fn close(app: &mut App) {
    let Some(f) = app.assist.take() else {
        return;
    };
    // An unconfirmed preview is cancelled so the frozen payload is dropped server-side.
    if let Phase::Preview { request, .. } = &f.phase {
        app.command_on(
            f.machine,
            "assistant.cancel",
            json!({"request": request}),
            Pending::Assist(Reply::Cancel),
        );
    }
    app.mode = match f.origin {
        Origin::Track if app.track.is_some() => Mode::Popup(Popup::Track),
        Origin::Task if app.task_view.is_some() => Mode::Popup(Popup::Task),
        Origin::Peek(p) => Mode::Popup(Popup::Peek { pane: p }),
        _ => Mode::Normal,
    };
}

// ---- output rendering --------------------------------------------------------------------------------

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(_) => v
            .get("text")
            .or_else(|| v.get("title"))
            .or_else(|| v.get("summary"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string()),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn list(out: &mut String, head: &str, items: &[Value], f: impl Fn(&Value) -> String) {
    if items.is_empty() {
        return;
    }
    out.push_str(head);
    out.push('\n');
    for i in items {
        out.push_str(&format!("- {}\n", f(i)));
    }
}

/// The generated output as editable text (task details are applied to the form instead).
pub fn render_output(op: Op, o: &Value) -> String {
    let mut s = String::new();
    match op {
        Op::PaneTitle => s = st(o, "title").to_string(),
        Op::ReviewSummary => {
            s.push_str(st(o, "summary"));
            s.push_str("\n\n");
            list(&mut s, "Changes:", arr(o, "changes"), text_of);
            list(&mut s, "Validation:", arr(o, "validation"), |v| {
                format!("{} [{}]", text_of(v), st(v, "basis"))
            });
            list(&mut s, "Outstanding:", arr(o, "outstanding"), text_of);
            list(&mut s, "Risks:", arr(o, "risks"), text_of);
        }
        Op::Briefing => {
            list(&mut s, "Briefing:", arr(o, "items"), |v| {
                let u = st(v, "urgency");
                if u.is_empty() {
                    text_of(v)
                } else {
                    format!("[{u}] {}", text_of(v))
                }
            });
            let cov = text_of(o.get("coverage").unwrap_or(&Value::Null));
            if !cov.is_empty() {
                s.push_str(&format!("\nCoverage: {cov}\n"));
            }
        }
        Op::SuggestTaskDetails => {
            s.push_str(&format!("Title: {}\n", st(o, "title")));
            s.push_str(&format!("Objective: {}\n", st(o, "objective")));
            list(&mut s, "Constraints:", arr(o, "constraints"), text_of);
            list(
                &mut s,
                "Criteria (optional until you require them):",
                arr(o, "criteria"),
                text_of,
            );
            list(
                &mut s,
                "Suggested checks (not selected):",
                arr(o, "suggested_checks"),
                text_of,
            );
            let stop = st(o, "stop_at");
            if !stop.is_empty() {
                s.push_str(&format!("Stop at: {}\n", crate::tasks::stop_label(stop)));
            }
            list(&mut s, "Questions:", arr(o, "questions"), text_of);
        }
    }
    s.trim_end().to_string()
}

/// Fill the Track form with suggested details. Nothing is saved: the user still edits and
/// presses Track task.
pub fn apply_to_track(f: &mut crate::tasks::TrackForm, o: &Value) {
    let title = st(o, "title").trim();
    if !title.is_empty() {
        f.title = title.to_string();
        f.title_edited = true;
    }
    let obj = st(o, "objective").trim();
    if !obj.is_empty() {
        f.objective = obj.to_string();
    }
    let mut crit: Vec<String> = Vec::new();
    for c in arr(o, "constraints").iter().chain(arr(o, "criteria")) {
        let t = text_of(c).trim().to_string();
        if !t.is_empty() && !crit.contains(&t) && !f.criteria.contains(&t) {
            crit.push(t);
        }
    }
    f.criteria.extend(crit);
    if let Some(i) = crate::tasks::STOP_AT
        .iter()
        .position(|(k, _)| *k == st(o, "stop_at"))
    {
        f.stop = i;
    }
    f.assisted = true;
}

// ---- keys ----------------------------------------------------------------------------------------------

pub fn key(app: &mut App, ev: KeyEvent) {
    let Some(mut f) = app.assist.take() else {
        app.mode = Mode::Normal;
        return;
    };
    app.mode = Mode::Popup(Popup::Assist);
    if ev.kind == KeyKind::Release {
        app.assist = Some(f);
        return;
    }
    f.notice = None;
    let esc = ev.key == Key::Named(NamedKey::Escape);
    match std::mem::replace(&mut f.phase, Phase::Generating) {
        Phase::Preview {
            request,
            preview,
            scroll,
        } => match ev.key {
            Key::Char('y' | 'Y') | Key::Named(NamedKey::Enter) => {
                let digest = st(&preview, "digest").to_string();
                app.command_on(
                    f.machine,
                    "assistant.confirm",
                    json!({"request": request, "preview_digest": digest}),
                    Pending::Assist(Reply::Confirm { flow: f.id }),
                );
                f.phase = Phase::Confirming { request };
            }
            Key::Char('n' | 'N') | Key::Named(NamedKey::Escape) | Key::Char('q') => {
                f.phase = Phase::Preview {
                    request,
                    preview,
                    scroll,
                };
                app.assist = Some(f);
                close(app);
                app.toast("Nothing was sent");
                return;
            }
            Key::Char('j') | Key::Named(NamedKey::Down) => {
                f.phase = Phase::Preview {
                    request,
                    preview,
                    scroll: scroll.saturating_add(1),
                }
            }
            Key::Char('k') | Key::Named(NamedKey::Up) => {
                f.phase = Phase::Preview {
                    request,
                    preview,
                    scroll: scroll.saturating_sub(1),
                }
            }
            Key::Named(NamedKey::PageDown) | Key::Char(' ') => {
                f.phase = Phase::Preview {
                    request,
                    preview,
                    scroll: scroll.saturating_add(10),
                }
            }
            Key::Named(NamedKey::PageUp) => {
                f.phase = Phase::Preview {
                    request,
                    preview,
                    scroll: scroll.saturating_sub(10),
                }
            }
            _ => {
                f.phase = Phase::Preview {
                    request,
                    preview,
                    scroll,
                }
            }
        },
        Phase::NeedsConsent { reason } => match ev.key {
            Key::Char('g' | 'G') => {
                let mut p = json!({"operations": [f.op.as_str()]});
                if let Some(w) = &f.workspace {
                    p["workspace"] = json!(w);
                }
                app.command_on(
                    f.machine,
                    "assistant.consent",
                    p,
                    Pending::Assist(Reply::Consent { flow: f.id }),
                );
                f.phase = Phase::Granting;
            }
            _ if esc || ev.key == Key::Char('n') => {
                app.assist = Some(f);
                close(app);
                return;
            }
            _ => f.phase = Phase::NeedsConsent { reason },
        },
        Phase::Done {
            request,
            output,
            mut ed,
        } => {
            if esc {
                app.assist = Some(f);
                close(app);
                return;
            }
            match f.op {
                Op::SuggestTaskDetails if ev.key == Key::Named(NamedKey::Enter) => {
                    if let Some(t) = &mut app.track {
                        apply_to_track(t, &output);
                    }
                    app.assist = Some(f);
                    close(app);
                    return;
                }
                Op::PaneTitle if ev.key == Key::Named(NamedKey::Enter) => {
                    let title = ed.text().trim().to_string();
                    if let Some(pane) = f.inputs.get("pane").and_then(Value::as_str)
                        && !title.is_empty()
                    {
                        app.command_on(
                            f.machine,
                            "pane.rename",
                            json!({"pane": pane, "title": title}),
                            Pending::Assist(Reply::Applied { flow: f.id }),
                        );
                        f.notice = Some(format!("Renaming the pane to “{title}”…"));
                    }
                }
                Op::ReviewSummary | Op::Briefing if ctrl(&ev, 'd') => {
                    let text = ed.text();
                    let (scope, id) = match (f.op, &f.inputs) {
                        (Op::ReviewSummary, i) => ("task", st(i, "task").to_string()),
                        _ => ("workspace", f.workspace.clone().unwrap_or_default()),
                    };
                    let key = app.new_idempotency_key("draft-assist");
                    app.command_on(
                        f.machine,
                        "draft.create",
                        json!({"scope": scope, "id": id, "text": text, "title": f.op.title(), "idempotency_key": key}),
                        Pending::Assist(Reply::DraftSaved { flow: f.id }),
                    );
                    f.notice = Some("Saving as a draft (nothing is sent)…".into());
                }
                Op::SuggestTaskDetails => {}
                _ => {
                    ed.key(&ev);
                }
            }
            f.phase = Phase::Done {
                request,
                output,
                ed,
            };
        }
        other => {
            if esc || matches!(ev.key, Key::Char('q')) {
                // Waiting: the request keeps running server-side; cancel only if asked.
                if let Phase::Waiting { request, .. } | Phase::Confirming { request } = &other {
                    app.command_on(
                        f.machine,
                        "assistant.cancel",
                        json!({"request": request}),
                        Pending::Assist(Reply::Cancel),
                    );
                    app.toast("Assist request cancelled");
                }
                app.assist = Some(f);
                close(app);
                return;
            }
            f.phase = other;
        }
    }
    app.assist = Some(f);
}

pub fn on_paste(app: &mut App, text: &str) -> bool {
    if !matches!(app.mode, Mode::Popup(Popup::Assist)) {
        return false;
    }
    if let Some(f) = &mut app.assist
        && let Phase::Done { ed, .. } = &mut f.phase
        && f.op != Op::SuggestTaskDetails
    {
        ed.insert_str(text);
        return true;
    }
    false
}

// ---- replies, polling, events --------------------------------------------------------------------------

fn flow_mut(app: &mut App, id: u64) -> Option<&mut Flow> {
    app.assist.as_mut().filter(|f| f.id == id)
}

/// Map an `assistant.*` error to the phase it shows.
pub fn error_phase(e: &RpcErr) -> Phase {
    let cat = e
        .details
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("");
    match (cat, e.reason()) {
        _ if e.is_method_not_found() => {
            Phase::Disabled("This machine's server has no assistant (needs a newer server)".into())
        }
        ("disabled", _) => Phase::Disabled(OFF.into()),
        ("not_configured", _) => {
            Phase::Disabled(format!("Assistance isn't configured — {}", e.message))
        }
        (
            _,
            Some(
                r @ ("consent_required"
                | "consent_invalidated"
                | "operation_not_granted"
                | "context_class_not_granted"),
            ),
        ) => Phase::NeedsConsent { reason: r.into() },
        _ => Phase::Failed(e.message.clone()),
    }
}

fn get(app: &mut App, f: &Flow, request: &str) {
    app.command_on(
        f.machine,
        "assistant.get",
        json!({"request": request}),
        Pending::Assist(Reply::Get { flow: f.id }),
    );
}

pub fn on_reply(app: &mut App, _mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    match r {
        Reply::Cancel => {}
        Reply::Generate { flow } => {
            let Some(f) = flow_mut(app, flow) else { return };
            match res {
                Ok(x) => {
                    let request = x
                        .pointer("/request/id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let preview = x.get("preview").cloned().unwrap_or(Value::Null);
                    if x.get("requires_confirmation").and_then(Value::as_bool) == Some(false) {
                        f.phase = Phase::Waiting {
                            request: request.clone(),
                            auto: true,
                        };
                        let f2 = f.clone();
                        get(app, &f2, &request);
                    } else {
                        f.phase = Phase::Preview {
                            request,
                            preview,
                            scroll: 0,
                        };
                    }
                }
                Err(e) => f.phase = error_phase(&e),
            }
        }
        Reply::Consent { flow } => {
            let Some(f) = flow_mut(app, flow) else { return };
            match res {
                Ok(_) => {
                    f.phase = Phase::Generating;
                    f.notice = Some("Consent granted for this workspace".into());
                    let f2 = f.clone();
                    generate(app, &f2);
                }
                Err(e) => f.phase = error_phase(&e),
            }
        }
        Reply::Confirm { flow } => {
            let Some(f) = flow_mut(app, flow) else { return };
            let request = match &f.phase {
                Phase::Confirming { request } => request.clone(),
                _ => return,
            };
            match res {
                Ok(_) => {
                    f.phase = Phase::Waiting {
                        request,
                        auto: false,
                    };
                    f.last_poll = Some(Instant::now());
                }
                Err(e) => f.phase = error_phase(&e),
            }
        }
        Reply::Get { flow } => {
            let Some(f) = flow_mut(app, flow) else { return };
            let Phase::Waiting { request, .. } = &f.phase else {
                return;
            };
            let request = request.clone();
            match res {
                Ok(x) => {
                    let r = x.get("request").cloned().unwrap_or(Value::Null);
                    match st(&r, "state") {
                        "done" => {
                            let output = r.get("output").cloned().unwrap_or(Value::Null);
                            let text = render_output(f.op, &output);
                            let multiline = f.op != Op::PaneTitle;
                            f.phase = Phase::Done {
                                request,
                                ed: TextEditor::new(&text, multiline),
                                output,
                            };
                        }
                        s @ ("failed" | "cancelled" | "interrupted") => {
                            let msg = r
                                .pointer("/error/message")
                                .and_then(Value::as_str)
                                .unwrap_or(s)
                                .to_string();
                            f.phase = Phase::Failed(msg);
                        }
                        _ => {}
                    }
                }
                Err(e) => f.phase = error_phase(&e),
            }
        }
        Reply::Applied { flow } => {
            let Some(f) = flow_mut(app, flow) else {
                return;
            };
            f.notice = Some(match res {
                Ok(_) => "Pane renamed".into(),
                Err(e) => format!("✗ {}", e.message),
            });
        }
        Reply::DraftSaved { flow } => {
            let Some(f) = flow_mut(app, flow) else {
                return;
            };
            f.notice = Some(match res {
                Ok(_) => "Saved as a draft — :drafts to edit or send (nothing was sent)".into(),
                Err(e) => format!("✗ draft not saved: {}", e.message),
            });
        }
    }
}

/// Poll a confirmed request about once a second (events make it faster).
pub fn tick(app: &mut App) {
    let Some(f) = &mut app.assist else {
        return;
    };
    if let Phase::Waiting { request, .. } = &f.phase
        && f.last_poll.is_none_or(|t| t.elapsed() >= POLL)
    {
        f.last_poll = Some(Instant::now());
        let (f2, r) = (f.clone(), request.clone());
        get(app, &f2, &r);
    }
}

/// Pushed `assistant.*` events: refresh the waiting request now.
pub fn on_event(app: &mut App, mi: usize, kind: &str) {
    let Some(f) = &mut app.assist else {
        return;
    };
    if f.machine != mi || !kind.starts_with("assistant.request_") {
        return;
    }
    if let Phase::Waiting { request, .. } = &f.phase {
        f.last_poll = Some(Instant::now());
        let (f2, r) = (f.clone(), request.clone());
        get(app, &f2, &r);
    }
}

// ---- drawing ---------------------------------------------------------------------------------------------

pub fn draw(app: &App, g: &mut Grid) {
    let Some(f) = &app.assist else {
        return;
    };
    let t = app.theme;
    let mut a = Area::open(app, g, &format!("assist · {}", f.op.title()));
    if let Some(n) = &f.notice {
        a.line(n, t.bold(t.yellow));
    }
    match &f.phase {
        Phase::Generating => {
            a.line("Preparing the preview… (nothing is sent yet)", t.dim());
            a.footer("esc close", t.dim());
        }
        Phase::Granting => a.line("Recording consent…", t.dim()),
        Phase::Disabled(m) => {
            a.line(m, t.bold(t.yellow));
            if m == OFF {
                a.line(
                    "Set [assistant] enabled = true and a profile in config.toml (user config only).",
                    t.dim(),
                );
            }
            a.line(
                "The structured inbox, peek and task details keep working without it.",
                t.dim(),
            );
            a.footer("esc close", t.dim());
        }
        Phase::NeedsConsent { reason } => {
            a.line(CONSENT, t.bold(t.yellow));
            let ws = f
                .workspace
                .as_ref()
                .and_then(|w| {
                    app.machines[f.machine]
                        .model
                        .workspaces
                        .iter()
                        .find(|x| &x.id == w)
                })
                .map(|w| format!("{} ({})", w.display_name(), w.root_path))
                .unwrap_or_else(|| "this workspace".into());
            a.line(&format!("Workspace: {ws}"), t.text());
            a.line(
                &format!(
                    "Allows “{}” to send selected context from this workspace to the configured provider.",
                    f.op.title()
                ),
                t.text(),
            );
            a.line(
                "Default classes: selected text, structured state, review package (never the screen unless granted).",
                t.dim(),
            );
            a.line(&format!("(server: {reason})"), t.dim());
            a.footer("[g] grant consent   [esc] not now", t.dim());
        }
        Phase::Preview {
            preview, scroll, ..
        } => draw_preview(app, &mut a, preview, *scroll),
        Phase::Confirming { .. } => a.line("Confirming…", t.dim()),
        Phase::Waiting { auto, .. } => {
            if *auto {
                a.line(
                    "Sent without confirmation (auto_send is enabled for this operation in config and consent)",
                    t.s(t.yellow),
                );
            }
            a.line("Waiting for the model…", t.dim());
            a.footer("esc cancel the request", t.dim());
        }
        Phase::Done { ed, .. } => {
            a.line(
                "Generated draft — edit freely; nothing is applied or sent until you act",
                t.s(t.accent),
            );
            let footer = match f.op {
                Op::SuggestTaskDetails => "enter fill the Track form (still unsaved) · esc discard",
                Op::PaneTitle => "edit · enter rename the pane · esc discard",
                Op::ReviewSummary | Op::Briefing => {
                    "edit · ctrl+d save as draft (never sends) · esc close"
                }
            };
            let r = a.rest();
            ed.draw(a.g, r, t.text(), t.rev());
            a.footer(footer, t.dim());
        }
        Phase::Failed(m) => {
            a.line(&format!("✗ {m}"), t.s(t.red));
            a.footer("esc close", t.dim());
        }
    }
}

fn draw_preview(app: &App, a: &mut Area, p: &Value, scroll: u16) {
    let t = app.theme;
    a.line(
        "Preview — exactly this will be sent. Nothing leaves this machine until you confirm.",
        t.bold(t.yellow),
    );
    let cost = p
        .get("estimated_max_cost_usd")
        .and_then(Value::as_f64)
        .map(|c| format!(" · ≤ ${c:.4}"))
        .unwrap_or_else(|| " · cost unknown".into());
    a.line(
        &format!(
            "{} via {} → {} · runs on {}",
            st(p, "model"),
            st(p, "adapter"),
            st(p, "endpoint_host"),
            st(p, "execution_machine")
        ),
        t.text(),
    );
    a.line(
        &format!(
            "{} bytes · ~{} input tokens · max {} output tokens{cost}",
            p.get("bytes").and_then(Value::as_u64).unwrap_or(0),
            p.get("estimated_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            p.get("max_output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        ),
        t.text(),
    );
    let red = p.get("redactions").and_then(Value::as_u64).unwrap_or(0);
    if red > 0 {
        a.line(
            &format!("{red} redaction(s) · {}", st(p, "notice")),
            t.s(t.yellow),
        );
    }
    let omitted = arr(p, "omitted");
    if !omitted.is_empty() {
        a.line(
            &format!("{} source(s) omitted to fit limits", omitted.len()),
            t.dim(),
        );
    }
    let mut body: Vec<(String, bool)> = vec![("─── system ───".into(), true)];
    body.extend(st(p, "system").lines().map(|l| (l.to_string(), false)));
    body.push(("─── user ───".into(), true));
    body.extend(st(p, "user").lines().map(|l| (l.to_string(), false)));
    for (l, head) in body.into_iter().skip(scroll as usize) {
        a.line(&l, if head { t.dim() } else { t.text() });
    }
    a.footer(
        "[y] send exactly this   [n] cancel (nothing sent)   j/k scroll",
        t.bold(t.fg),
    );
}

#[cfg(test)]
#[path = "assist_tests.rs"]
mod tests;
