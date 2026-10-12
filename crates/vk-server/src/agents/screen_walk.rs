//! Answering screen pickers and pointer-marked questions by "walk, verify, commit": re-read
//! the pane, check the picker is still the one the interaction describes, move one key at a
//! time and confirm each move on screen, then send the commit key bound to the last verified
//! read. Digits are never pressed (in some pickers a digit selects and persists at once).

use super::Harness;
use super::screen::{self, Dialog};
use super::screen_picker::{Goal, MenuKeys, Picker, Row, Step, next_step, state_of};
use crate::Server;
use crate::pane::PaneRt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vk_proto::model::*;

/// How long one move may take to show on screen.
const STEP_TIMEOUT: Duration = Duration::from_millis(1500);
/// How long the picker may take to close after the commit key.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the question dialog must stay off screen to count as closed.
const GONE_SETTLE: Duration = Duration::from_millis(250);

/// A pointer-marked screen question as a picker view, so it is walked like one. Option ids
/// stay the question's (`"1"`, `"2"`, …) — they are ids, never keys to press.
pub fn question_view(d: &Dialog) -> Option<Picker> {
    let at = d.pointer?;
    let rows: Vec<Row> = d
        .options
        .iter()
        .map(|(n, l, _)| Row {
            id: n.to_string(),
            label: l.clone(),
            description: None,
            checked: None,
        })
        .collect();
    let pointer = d.options.iter().position(|(n, _, _)| *n == at)?;
    Some(Picker {
        name: "question".into(),
        title: d.title.clone(),
        body: None,
        rows,
        pointer: Some(pointer),
        multi: false,
        scrolls: false,
        adjust: None,
        keys: MenuKeys {
            up: Some("up".into()),
            down: Some("down".into()),
            confirm: Some("enter".into()),
            ..Default::default()
        },
        signature: d.fingerprint.clone(),
    })
}

/// Should this on-screen dialog be answered by walking rather than by planned keys?
pub fn walks(h: Harness, d: &Dialog, it: &Interaction) -> bool {
    d.picker.is_some()
        || (it.kind == InteractionKind::Question
            && d.pointer.is_some()
            && matches!(
                h.base(),
                Harness::Claude | Harness::Codex | Harness::Pi | Harness::Omp
            ))
}

/// The answer as a walk goal. `rows` resolves labels to ids (questions accepted labels).
pub fn goal_of(it: &Interaction, answer: &Answer) -> Goal {
    let mut g = Goal {
        cancel: answer.decision == Some(Decision::Cancel),
        ..Default::default()
    };
    let opts: Vec<&QuestionOption> = it
        .questions
        .first()
        .map(|q| q.options.iter().collect())
        .unwrap_or_default();
    let resolve = |want: &String| -> String {
        opts.iter()
            .find(|o| o.id == *want)
            .or_else(|| opts.iter().find(|o| o.label == *want))
            .or_else(|| opts.iter().find(|o| o.label.starts_with(want.as_str())))
            .map(|o| o.id.clone())
            .unwrap_or_else(|| want.clone())
    };
    let multi = it.questions.first().is_some_and(|q| q.multi);
    for (q, ids) in &answer.choices {
        match q.as_str() {
            "adjust" => g.adjust = ids.first().cloned(),
            "scope" => g.persist = ids.iter().any(|s| s == "default"),
            _ if multi => g.checked = Some(ids.iter().map(resolve).collect()),
            _ => g.target = ids.first().map(resolve),
        }
    }
    g
}

/// Translate a goal in the interaction's option ids into the ids of the rows `view` shows.
///
/// A picker's options are its screen rows already. A question's options may come from a
/// structured source (hook-backed questions use labels such as `Stop here` as ids) while the
/// screen view numbers its rows, so each option goes id → label → the one row showing that
/// label: an exact label first, then the same label ignoring case and spacing, then a unique
/// prefix match. An option no row (or more than one row) matches is refused.
pub fn to_screen_rows(
    it: &Interaction,
    view: &Picker,
    goal: &mut Goal,
    order: &mut Vec<String>,
) -> Result<(), String> {
    if it.kind == InteractionKind::Picker {
        return Ok(());
    }
    let opts: Vec<&QuestionOption> = it
        .questions
        .first()
        .map(|q| q.options.iter().collect())
        .unwrap_or_default();
    let norm = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let row_for = |id: &str| -> Option<String> {
        let label = opts
            .iter()
            .find(|o| o.id == id)
            .map(|o| o.label.as_str())
            .unwrap_or(id);
        let one = |m: Vec<&Row>| (m.len() == 1).then(|| m[0].id.clone());
        let (want, rows) = (norm(label), &view.rows);
        one(rows.iter().filter(|r| r.label == label).collect())
            .or_else(|| one(rows.iter().filter(|r| norm(&r.label) == want).collect()))
            .or_else(|| {
                one(rows
                    .iter()
                    .filter(|r| {
                        let have = norm(&r.label);
                        !want.is_empty() && (have.starts_with(&want) || want.starts_with(&have))
                    })
                    .collect())
            })
    };
    let map = |id: &String| row_for(id).ok_or_else(|| format!("option {id} is not on screen"));
    if let Some(t) = &goal.target {
        goal.target = Some(map(t)?);
    }
    if let Some(c) = &goal.checked {
        goal.checked = Some(c.iter().map(map).collect::<Result<_, _>>()?);
    }
    *order = order.iter().filter_map(|id| row_for(id)).collect();
    Ok(())
}

/// The picker (or walkable question) currently on the pane.
fn read(h: Harness, rt: &PaneRt) -> Option<Picker> {
    let text = rt.screen.lock().unwrap().engine.screen_text();
    let d = screen::evaluate(h, &text).dialog?;
    match d.picker {
        Some(p) => Some(p),
        None if d.kind == InteractionKind::Question => question_view(&d),
        None => None,
    }
}

async fn send_key(server: &Arc<Server>, rt: &PaneRt, pane: &str, key: &str) -> bool {
    server.agents.lock_input(pane);
    let modes = crate::render::input_modes(server, pane);
    let Ok(ev) = vk_term::keygrammar::parse_key(key) else {
        return false;
    };
    let bytes = vk_term::encode::encode_key(&ev, &modes);
    rt.input(server.next_internal_input_id(), bytes).await
        != vk_proto::holder::InputStatus::ChildExited
}

/// Walk, verify, commit. Holds the pane's input lock for the whole walk.
pub async fn walk(
    server: &Arc<Server>,
    h: Harness,
    rt: &PaneRt,
    it: &Interaction,
    signature: &str,
    answer: &Answer,
) -> (DeliveryState, Option<String>) {
    let mut goal = goal_of(it, answer);
    let mut order: Vec<String> = it
        .questions
        .first()
        .map(|q| q.options.iter().map(|o| o.id.clone()).collect())
        .unwrap_or_default();
    server.agents.lock_input(&it.pane);
    let mapped = match read(h, rt) {
        Some(view) => to_screen_rows(it, &view, &mut goal, &mut order),
        None => Err("picker_changed: the picker is no longer on screen".into()),
    };
    let out = match mapped {
        Ok(()) => walk_locked(server, h, rt, it, signature, &goal, &order).await,
        Err(r) => (DeliveryState::Failed, Some(r)),
    };
    server.agents.unlock_input(&it.pane);
    out
}

async fn walk_locked(
    server: &Arc<Server>,
    h: Harness,
    rt: &PaneRt,
    it: &Interaction,
    signature: &str,
    goal: &Goal,
    order: &[String],
) -> (DeliveryState, Option<String>) {
    let failed = |r: String| (DeliveryState::Failed, Some(r));
    // Bounded: every row (twice for multi-select toggles) plus the adjuster's values.
    let max_steps = 2 * order.len().max(10) + 12;
    for _ in 0..max_steps {
        let Some(p) = read(h, rt) else {
            return failed("picker_changed: the picker is no longer on screen".into());
        };
        if p.signature != signature {
            return failed("picker_changed: the screen shows a different picker".into());
        }
        match next_step(&p, goal, order) {
            Step::Fail(r) => return failed(r),
            Step::Key(k) => {
                let before = state_of(&p);
                if !send_key(server, rt, &it.pane, &k).await {
                    return failed("pane exited".into());
                }
                let deadline = Instant::now() + STEP_TIMEOUT;
                let mut moved = false;
                while Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    match read(h, rt) {
                        Some(now) if now.signature != signature => {
                            return failed(
                                "picker_changed: the screen changed while walking".into(),
                            );
                        }
                        Some(now) if state_of(&now) != before => {
                            moved = true;
                            break;
                        }
                        Some(_) => {}
                        None => {
                            return failed(
                                "picker_changed: the picker closed while walking".into(),
                            );
                        }
                    }
                }
                if !moved {
                    return failed(format!("the picker did not respond to {k}"));
                }
            }
            Step::Commit(k) => {
                if !send_key(server, rt, &it.pane, &k).await {
                    return failed("pane exited".into());
                }
                let deadline = Instant::now() + COMMIT_TIMEOUT;
                while Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    match read(h, rt) {
                        // Closed, or replaced by a follow-up step (e.g. a confirmation).
                        None => return (DeliveryState::Delivered, None),
                        Some(now) if now.signature != signature => {
                            return (DeliveryState::Delivered, None);
                        }
                        Some(_) => {}
                    }
                }
                return (
                    DeliveryState::DeliveryUnknown,
                    Some("picker still visible".into()),
                );
            }
        }
    }
    failed("the walk did not converge".into())
}

/// Claude's `AskUserQuestion` dialog on the pane, if it shows one.
fn read_ask(h: Harness, rt: &PaneRt) -> Option<super::screen_ask::AskScreen> {
    let text = rt.screen.lock().unwrap().engine.screen_text();
    screen::evaluate(h, &text).dialog.and_then(|d| d.ask)
}

/// Answer Claude's `AskUserQuestion` dialog: for each question, check the visible question is
/// one of the interaction's, walk the pointer to the chosen option and select it, waiting for
/// the dialog to move on; with several questions, finish on the review's **Submit answers**.
/// Every key is planned from a fresh read and every step confirmed on screen; a surprise stops
/// the walk with a reason instead of selecting into another dialog.
pub async fn walk_ask(
    server: &Arc<Server>,
    h: Harness,
    rt: &PaneRt,
    it: &Interaction,
    answer: &Answer,
) -> (DeliveryState, Option<String>) {
    server.agents.lock_input(&it.pane);
    let out = walk_ask_locked(server, h, rt, it, answer).await;
    server.agents.unlock_input(&it.pane);
    out
}

async fn walk_ask_locked(
    server: &Arc<Server>,
    h: Harness,
    rt: &PaneRt,
    it: &Interaction,
    answer: &Answer,
) -> (DeliveryState, Option<String>) {
    use super::screen_ask::{AskStep, needs_submit, next_step, wanted};
    let want = wanted(it, answer);
    let mut done = vec![false; want.len()];
    // Once a key was sent the pane may hold part of the answer: report "unknown", not "failed".
    let mut typed = false;
    let stop = |typed: bool, r: String| {
        if typed {
            (
                DeliveryState::DeliveryUnknown,
                Some(format!(
                    "{r}; the dialog may be partly answered, check the pane"
                )),
            )
        } else {
            (DeliveryState::Failed, Some(r))
        }
    };
    // Bounded: per question a walk over its rows, a tab switch and the selection, plus the review.
    let max_steps = 12 * (want.len() + 1) + 12;
    for _ in 0..max_steps {
        let Some(a) = read_ask(h, rt) else {
            return stop(
                typed,
                "dialog_changed: the question dialog is no longer on screen".into(),
            );
        };
        if !super::screen_ask::matches(&a, it) {
            return stop(
                typed,
                "dialog_changed: the visible dialog is not this interaction".into(),
            );
        }
        let submit = needs_submit(&a, it);
        let step = next_step(&a, it, &want, &done);
        let (key, selects) = match step {
            AskStep::Fail(r) => return stop(typed, r),
            AskStep::Key(k) => (k, None),
            AskStep::Select { key, question } => (key, Some(question)),
        };
        let before = a.state();
        if !send_key(server, rt, &it.pane, &key).await {
            return (DeliveryState::Failed, Some("pane exited".into()));
        }
        typed = true;
        let timeout = if selects.is_some() {
            COMMIT_TIMEOUT
        } else {
            STEP_TIMEOUT
        };
        let deadline = Instant::now() + timeout;
        let mut now = None;
        let mut changed = false;
        // The dialog counts as closed once no dialog has been read for a while (a redraw can
        // briefly show a partial frame).
        let mut gone_since: Option<Instant> = None;
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(60)).await;
            now = read_ask(h, rt);
            match &now {
                None => {
                    let since = *gone_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= GONE_SETTLE {
                        changed = true;
                        break;
                    }
                }
                Some(n) => {
                    gone_since = None;
                    // A selection must move to another question or the review, not just the
                    // pointer.
                    if n.state() != before
                        && (selects.is_none() || n.view != a.view || n.title != a.title)
                    {
                        changed = true;
                        break;
                    }
                }
            }
        }
        if !changed && gone_since.is_some() {
            changed = true;
        }
        if !changed {
            return stop(
                typed,
                format!("the question dialog did not respond to {key}"),
            );
        }
        let Some(question) = selects else {
            continue;
        };
        match question {
            Some(k) => done[k] = true,
            // Submit answers: the dialog must close.
            None => {
                return if now.is_none() {
                    (DeliveryState::Delivered, None)
                } else {
                    (
                        DeliveryState::DeliveryUnknown,
                        Some("the question dialog is still open after Submit answers".into()),
                    )
                };
            }
        }
        if done.iter().all(|d| *d) && !submit {
            // A single question: Claude submits on the selection (the dialog closes). A view
            // of a larger dialog (screen-sourced): it moved on to the next question.
            return (DeliveryState::Delivered, None);
        }
        if now.is_none() {
            return (
                DeliveryState::DeliveryUnknown,
                Some(
                    "the question dialog closed before every answer was entered; check the pane"
                        .into(),
                ),
            );
        }
    }
    stop(
        typed,
        "the walk through the question dialog did not converge".into(),
    )
}
