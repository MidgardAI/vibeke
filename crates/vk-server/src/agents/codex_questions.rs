//! Codex's asynchronous follow-up questions. The rollout supplies their identity and content;
//! the queue/editor on screen confirms they still wait. Answers walk the editor only after an
//! explicit interaction answer. A running tool or a new steer does not dismiss these questions.

use super::*;
use crate::pane::PaneRt;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use vk_agents::transcript::PendingQuestion;

const PREFIX: &str = "codex-question:";
const OTHER: &str = "Other (write an answer)";
const INPUT: &str = "Type your answer";
const WAIT: Duration = Duration::from_millis(1500);
type WalkLock = std::sync::Weak<tokio::sync::Mutex<()>>;
static WALKS: LazyLock<Mutex<HashMap<String, WalkLock>>> = LazyLock::new(Default::default);

/// Refresh the ordinary interaction input lock while awaiting redraws. Its two-second stale
/// limit must not let client input join a longer walk. Dropping the guard always releases it.
struct InputLock {
    server: Arc<Server>,
    pane: String,
}
impl InputLock {
    fn new(server: &Arc<Server>, pane: &str) -> Self {
        server.agents.lock_input(pane);
        Self {
            server: server.clone(),
            pane: pane.into(),
        }
    }
}
impl Drop for InputLock {
    fn drop(&mut self) {
        self.server.agents.unlock_input(&self.pane);
    }
}

static COUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\?\s+(\d+) questions?(?:\s.*)?$").unwrap());
static PROGRESS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+) of (\d+)$").unwrap());
static ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(›)?\s*(\d{1,2})\.\s+(\S.*?)\s*$").unwrap());

pub(super) fn is_async(it: &Interaction) -> bool {
    it.native_ref
        .as_deref()
        .is_some_and(|r| r.starts_with(PREFIX))
}

pub(super) fn queued_count(text: &str) -> Option<usize> {
    let lines: Vec<_> = text.lines().collect();
    let tail = &lines;
    let at = tail
        .iter()
        .rposition(|l| l.trim().trim_start_matches('•').trim() == "Queued follow-up inputs")?;
    tail[at + 1..]
        .iter()
        .take_while(|l| !l.trim_start().starts_with('›'))
        .find_map(|l| COUNT.captures(l.trim()).and_then(|c| c[1].parse().ok()))
        .filter(|n| *n > 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Editor {
    index: usize,
    total: usize,
    title: String,
    rows: Vec<String>,
    pointer: Option<usize>,
    input: String,
    footer: String,
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Codex renders progress, the full question, numbered choices (including Other), then a
/// submit/skip/navigation footer. A partial frame, clipped title or unknown footer fails closed.
fn editor(text: &str) -> Option<Editor> {
    let lines: Vec<_> = text.lines().collect();
    let header = lines
        .iter()
        .rposition(|l| l.trim().trim_start_matches('•').trim() == "Queued follow-up inputs")?;
    if lines.len() - header > 60 {
        return None;
    }
    let progress = lines[header + 1..]
        .iter()
        .position(|l| PROGRESS.is_match(l.trim()))
        .map(|i| header + 1 + i);
    let (start, index, total) = if let Some(start) = progress {
        let c = PROGRESS.captures(lines[start].trim())?;
        (
            start,
            c[1].parse::<usize>().ok()?,
            c[2].parse::<usize>().ok()?,
        )
    } else {
        (header, 1, 1)
    };
    if index == 0 || index > total || total > 256 {
        return None;
    }
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.contains("submit") && l.contains("skip"))?
        + start
        + 1;
    let footer = lines[end..]
        .iter()
        .take(3)
        .map(|l| l.trim())
        .take_while(|l| {
            l.contains("submit")
                || l.contains("skip")
                || l.contains("main prompt")
                || l.contains("prev question")
                || l.contains("next question")
                || l.contains("queued messages")
        })
        .collect::<Vec<_>>()
        .join(" ");
    if !footer.contains("main prompt") && !footer.contains("prev question") {
        return None;
    }
    let body = &lines[start + 1..end];
    let first_title = body
        .iter()
        .position(|l| !l.trim().is_empty() && !l.trim().starts_with('↳'))?;
    let title_end = body[first_title..]
        .iter()
        .position(|l| l.trim().is_empty())?
        + first_title;
    let title = norm(&body[first_title..title_end].join(" "));
    if title.is_empty() {
        return None;
    }
    let input = norm(&body[title_end..].join(" "));
    let mut rows: Vec<String> = vec![];
    let mut pointer = None;
    for l in &body[title_end..] {
        let t = l.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(c) = ROW.captures(l) {
            let n = c[2].parse::<usize>().ok()?;
            if n != rows.len() + 1 {
                return None;
            }
            if c.get(1).is_some() {
                pointer = Some(rows.len());
            }
            rows.push(c[3].to_string());
        } else if rows.is_empty() {
            // A free-text question has an inline composer instead of option rows.
        } else {
            let last = rows.last_mut()?;
            last.push(' ');
            last.push_str(t);
        }
    }
    if !rows.is_empty() && pointer.is_none() {
        return None;
    }
    Some(Editor {
        index,
        total,
        title,
        rows,
        pointer,
        input,
        footer,
    })
}

fn has_questions(text: &str) -> bool {
    queued_count(text).is_some() || editor(text).is_some()
}

fn queued_steers(text: &str) -> bool {
    text.lines().rev().any(|l| l.trim().starts_with('↳'))
        && text.contains("Queued follow-up inputs")
}

fn interaction_id(run: &str, question: &str) -> String {
    format!(
        "cq{}",
        Sha256::digest(json!([run, question]).to_string().as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

fn screen_pending(
    server: &Server,
    run: &str,
    mut pending: Vec<PendingQuestion>,
) -> Vec<PendingQuestion> {
    // Codex removes an accepted local answer from its editor before it writes the next model
    // request. These known submissions no longer occupy a position in the visible queue.
    let submitted = server.with_core(|c| {
        c.model
            .interactions
            .iter()
            .filter(|i| {
                i.run == run
                    && i.status == InteractionStatus::Answered
                    && i.delivery == DeliveryState::DecisionRecorded
            })
            .flat_map(|i| i.questions.iter().map(|q| q.id.clone()))
            .collect::<std::collections::HashSet<_>>()
    });
    pending.retain(|q| !submitted.contains(&q.id));
    pending
}

pub(super) fn sync_answers(
    server: &Arc<Server>,
    run: &AgentRun,
    answers: &HashMap<String, String>,
) {
    for (id, value) in answers {
        let id = interaction_id(&run.id, id);
        let it = server.with_core(|c| {
            c.store
                .find::<Interaction>("interaction", &id)
                .ok()
                .flatten()
        });
        if let Some(it) = it
            && it.status == InteractionStatus::Answered
            && matches!(
                it.delivery,
                DeliveryState::DecisionRecorded | DeliveryState::DeliveryUnknown
            )
            && let Some(answer) = &it.answer
            && answer_text(&it, answer).is_ok_and(|(expected, _)| expected.trim() == value.trim())
        {
            set_delivery(server, &id, DeliveryState::Delivered, None);
        }
    }
}

pub(super) fn sync(server: &Arc<Server>, run: &AgentRun, pending: &[PendingQuestion]) {
    if headless::is_headless(run) {
        return;
    }
    let text = server
        .pane_rt(&run.pane)
        .map(|rt| rt.screen.lock().unwrap().engine.screen_text());
    // Never resurrect historical questions whose turn is no longer visible in this process.
    let visible = text
        .as_deref()
        .is_some_and(|t| has_questions(t) || queued_steers(t));
    let existing = server.with_core(|c| {
        c.model
            .interactions
            .iter()
            .filter(|i| i.run == run.id && is_async(i))
            .cloned()
            .collect::<Vec<_>>()
    });
    for it in &existing {
        if it.status == InteractionStatus::Open
            && !pending
                .iter()
                .any(|q| it.native_ref.as_deref() == Some(format!("{PREFIX}{}", q.id).as_str()))
        {
            resolve(
                server,
                &it.id,
                InteractionStatus::ResolvedElsewhere,
                "native:Codex question answered or turn ended",
            );
        }
    }
    let expected = screen_pending(server, &run.id, pending.to_vec());
    let count = text
        .as_deref()
        .and_then(|t| queued_count(t).or_else(|| editor(t).map(|e| e.total)));
    if count == Some(expected.len()) {
        // A reply can reach the rollout after a temporary count mismatch. Restore inbox
        // answers once the visible order agrees again, including after another client replies.
        server.with_core(|c| {
            let items = c
                .model
                .interactions
                .iter()
                .filter(|i| {
                    i.run == run.id
                        && is_async(i)
                        && i.status == InteractionStatus::Open
                        && !i.answerable
                        && expected.iter().any(|q| {
                            !q.title.trim().is_empty()
                                && i.questions.first().is_some_and(|asked| asked.id == q.id)
                        })
                })
                .cloned()
                .collect::<Vec<_>>();
            let changed = !items.is_empty();
            let mut tx = Tx::new();
            for mut it in items {
                it.answerable = true;
                it.body_md = Some(
                    "Codex can keep working while this question waits for your answer.".into(),
                );
                tx.event(
                    "interaction.updated",
                    json!({"interaction":it.id,"run":it.run,"pane":it.pane}),
                    json!({"answerable":true}),
                );
                tx.interaction(it);
            }
            if changed {
                let _ = server.commit(c, tx);
            }
        });
    }
    if !visible {
        return;
    }
    for q in pending {
        let native = format!("{PREFIX}{}", q.id);
        if existing
            .iter()
            .any(|i| i.native_ref.as_deref() == Some(&native))
        {
            continue;
        }
        let title = if q.title.trim().is_empty() {
            "Codex has a follow-up question".into()
        } else {
            vk_redact::redact(&q.title).into_owned()
        };
        let mut it = Interaction {
            id: interaction_id(&run.id, &q.id),
            handle: String::new(),
            run: run.id.clone(),
            pane: run.pane.clone(),
            kind: InteractionKind::Question,
            status: InteractionStatus::Open,
            title: title.clone(),
            body_md: Some(
                "Codex can keep working while this question waits for your answer.".into(),
            ),
            action: None,
            questions: vec![Question {
                id: q.id.clone(),
                prompt: title,
                header: None,
                multi: false,
                options: q
                    .options
                    .iter()
                    .enumerate()
                    .map(|(n, label)| QuestionOption {
                        id: n.to_string(),
                        label: vk_redact::redact(label).into_owned(),
                        description: None,
                        selected: false,
                    })
                    .collect(),
                allow_free_text: true,
            }],
            plan_md: None,
            answer_channel: AnswerChannel::Keystrokes,
            native_ref: Some(native),
            source: StateSource::Structured,
            confidence: 1.0,
            answerable: !q.title.trim().is_empty(),
            gate: false,
            decision_rev: 0,
            delivery: DeliveryState::None,
            delivery_error: None,
            answer: None,
            answered_by: None,
            answer_key: None,
            opened_at_ms: now_ms(),
            answered_at_ms: None,
            picker: None,
        };
        let committed = server.with_core(|c| {
            // A screen evaluation and a tailer pass may meet here at the same time.
            if c.store
                .find::<Interaction>("interaction", &it.id)
                .ok()
                .flatten()
                .is_some()
            {
                return false;
            }
            it.handle = c.next_interaction_handle();
            let mut tx = Tx::new();
            tx.counters = true;
            tx.event(
                "interaction.opened",
                json!({"interaction": it.id, "pane": it.pane, "run": run.id}),
                json!({"kind": "question", "source": "transcript", "confidence": 1.0}),
            );
            tx.interaction(it.clone());
            server.commit(c, tx).is_ok()
        });
        if committed {
            notify_interaction(server, &it, run);
        }
    }
}

/// Expanded editors belong to the transcript interactions, not a second generic screen picker.
pub(super) fn on_screen(server: &Arc<Server>, run: &AgentRun, text: &str) -> bool {
    let expanded = editor(text).is_some();
    let Some(pending) = tailer::pending_questions(&run.id) else {
        return expanded;
    };
    sync(server, run, &pending);
    let pending = screen_pending(server, &run.id, pending);
    let missing_key = format!("codex-question:{}", run.pane);
    let count = queued_count(text).or_else(|| editor(text).map(|e| e.total));
    if count.is_some_and(|n| n < pending.len()) || (count.is_none() && queued_steers(text)) {
        let mismatch_key = format!("codex-question-mismatch:{}", run.pane);
        let mut inner = server.agents.inner.lock().unwrap();
        let old = inner.no_dialog_since.get(&mismatch_key).copied();
        let since = *inner
            .no_dialog_since
            .entry(mismatch_key)
            .or_insert_with(Instant::now);
        drop(inner);
        if old.is_none() {
            let (srv, pane) = (server.clone(), run.pane.clone());
            tokio::spawn(async move {
                tokio::time::sleep(WAIT + Duration::from_millis(150)).await;
                srv.agents.on_screen(&srv, &pane);
            });
        }
        if since.elapsed() >= WAIT {
            server.with_core(|c| {
                let items = c.model.interactions.iter().filter(|i| i.run == run.id && is_async(i) && i.status == InteractionStatus::Open && i.answerable).cloned().collect::<Vec<_>>();
                let mut tx = Tx::new();
                for mut it in items {
                    it.answerable = false;
                    it.body_md = Some("Codex changed the question queue. Answer the remaining questions in the pane.".into());
                    tx.event("interaction.updated", json!({"interaction":it.id,"run":it.run,"pane":it.pane}), json!({"answerable":false}));
                    tx.interaction(it);
                }
                if !tx.interactions.is_empty() { let _ = server.commit(c, tx); }
            });
        }
    } else {
        server
            .agents
            .inner
            .lock()
            .unwrap()
            .no_dialog_since
            .remove(&format!("codex-question-mismatch:{}", run.pane));
    }
    if has_questions(text) || queued_steers(text) {
        server
            .agents
            .inner
            .lock()
            .unwrap()
            .no_dialog_since
            .remove(&missing_key);
    } else if text
        .trim_end()
        .lines()
        .rev()
        .take(8)
        .any(|l| l.trim() == "›" || l.contains("Ask Codex to"))
    {
        // Skipping a question is local to Codex's TUI. Wait through partial redraws before
        // retiring cards. A durable id keeps a transcript replay from opening them again.
        let mut inner = server.agents.inner.lock().unwrap();
        let old = inner.no_dialog_since.get(&missing_key).copied();
        let since = *inner
            .no_dialog_since
            .entry(missing_key)
            .or_insert_with(Instant::now);
        drop(inner);
        if old.is_none() {
            let (srv, pane) = (server.clone(), run.pane.clone());
            tokio::spawn(async move {
                tokio::time::sleep(WAIT + Duration::from_millis(150)).await;
                srv.agents.on_screen(&srv, &pane);
            });
        }
        if since.elapsed() >= WAIT {
            let ids = server.with_core(|c| {
                c.model
                    .interactions
                    .iter()
                    .filter(|i| {
                        i.run == run.id && is_async(i) && i.status == InteractionStatus::Open
                    })
                    .map(|i| i.id.clone())
                    .collect::<Vec<_>>()
            });
            for id in ids {
                resolve(
                    server,
                    &id,
                    InteractionStatus::ResolvedElsewhere,
                    "native:Codex question dismissed in pane",
                );
            }
        }
    }
    expanded
}

fn answer_text(it: &Interaction, a: &Answer) -> Result<(String, bool), String> {
    let q = it
        .questions
        .first()
        .ok_or("selection_mismatch: missing question")?;
    if a.decision.is_some() {
        return Err("selection_mismatch: choose an option or enter an answer".into());
    }
    let picked: Vec<_> = a
        .choices
        .iter()
        .flat_map(|(id, ids)| ids.iter().map(move |v| (id, v)))
        .collect();
    if picked.len() > 1 {
        return Err("selection_mismatch: choose one answer".into());
    }
    let choice = picked
        .first()
        .map(|(id, v)| {
            if *id != &q.id {
                return Err("selection_mismatch: unknown question".to_string());
            }
            q.options
                .iter()
                .find(|o| o.id == **v || o.label == **v)
                .map(|o| o.label.clone())
                .ok_or_else(|| "selection_mismatch: unknown answer".to_string())
        })
        .transpose()?;
    let text = a.text.as_deref().filter(|s| !s.trim().is_empty());
    match (choice, text) {
        (Some(s), Some(text)) => Ok((format!("{s}\n\n{text}"), true)),
        (Some(s), None) => Ok((s, false)),
        (None, Some(s)) if !s.chars().any(|c| c.is_control() && c != '\n') => {
            Ok((s.to_string(), true))
        }
        _ => Err("selection_mismatch: an answer is required".into()),
    }
}

pub(super) fn validate(it: &Interaction, a: &Answer) -> Result<(), String> {
    let (value, free) = answer_text(it, a)?;
    if free
        && (value.chars().any(|c| c.is_control() && c != '\n')
            || value.len() > 512
            || value.lines().count() > 8
            || value.lines().any(|l| ROW.is_match(l)))
    {
        return Err("answer_in_pane: long or numbered answers must be typed in the pane".into());
    }
    Ok(())
}

fn read(rt: &PaneRt) -> String {
    rt.screen.lock().unwrap().engine.screen_text()
}

async fn key(server: &Arc<Server>, rt: &PaneRt, pane: &str, key: &str) -> bool {
    server.agents.lock_input(pane);
    let Ok(ev) = vk_term::keygrammar::parse_key(key) else {
        return false;
    };
    let bytes = vk_term::encode::encode_key(&ev, &crate::render::input_modes(server, pane));
    rt.input(server.next_internal_input_id(), bytes).await
        != vk_proto::holder::InputStatus::ChildExited
}

pub(super) async fn deliver(
    server: &Arc<Server>,
    rt: &PaneRt,
    it: &Interaction,
    a: &Answer,
) -> (DeliveryState, Option<String>) {
    let Ok((value, free)) = answer_text(it, a) else {
        return (DeliveryState::Failed, Some("selection_mismatch".into()));
    };
    let walk = {
        let mut locks = WALKS.lock().unwrap();
        locks.retain(|_, lock| lock.strong_count() > 0);
        let lock = locks
            .get(&it.run)
            .and_then(std::sync::Weak::upgrade)
            .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
        locks.insert(it.run.clone(), Arc::downgrade(&lock));
        lock
    };
    let _walk = walk.lock().await;
    let _input = InputLock::new(server, &it.pane);
    let opened = editor(&read(rt)).is_none();
    let bounded = tokio::time::timeout(
        Duration::from_secs(15),
        deliver_locked(server, rt, it, &value, free),
    );
    tokio::pin!(bounded);
    let mut pulse = tokio::time::interval(Duration::from_millis(200));
    let out = loop {
        tokio::select! {
            out = &mut bounded => break out.unwrap_or_else(|_| (DeliveryState::DeliveryUnknown,
                Some("answer_in_pane: the question walk timed out; check the pane for a draft".into()))),
            _ = pulse.tick() => server.agents.lock_input(&it.pane),
        }
    };
    if opened {
        // Restoration is outside the walk timeout, so a timed-out walk still returns to the
        // main composer when Codex shows known navigation keys.
        let restore =
            tokio::time::timeout(Duration::from_secs(4), restore_prompt(server, rt, &it.pane));
        tokio::pin!(restore);
        loop {
            tokio::select! { _ = &mut restore => break, _ = pulse.tick() => server.agents.lock_input(&it.pane) }
        }
    }
    out
}

async fn restore_prompt(server: &Arc<Server>, rt: &PaneRt, pane: &str) {
    // Back visits earlier questions, then returns to the main composer without submitting or
    // discarding any draft. Never use Escape: in Codex it may interrupt the running turn.
    for _ in 0..32 {
        let Some(before) = editor(&read(rt)) else {
            return;
        };
        if !before.footer.contains("⇧→ main prompt") && !before.footer.contains("⇧→ prev question")
        {
            return;
        }
        if !key(server, rt, pane, "shift+right").await {
            return;
        }
        let deadline = Instant::now() + WAIT;
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            match editor(&read(rt)) {
                None => return,
                Some(next) if next.index < before.index => break,
                _ if Instant::now() >= deadline => return,
                _ => {}
            }
        }
    }
}

async fn deliver_locked(
    server: &Arc<Server>,
    rt: &PaneRt,
    it: &Interaction,
    value: &str,
    free: bool,
) -> (DeliveryState, Option<String>) {
    let mut touched = false;
    let fail = |touched, why: &str| {
        (
            if touched {
                DeliveryState::DeliveryUnknown
            } else {
                DeliveryState::Failed
            },
            Some(format!("answer_in_pane: {why}")),
        )
    };
    let current_run =
        || server.with_core(|c| c.run_for_pane(&it.pane).is_some_and(|r| r.id == it.run));
    if !current_run() {
        return fail(false, "the pane is running a different agent session");
    }
    let pending = screen_pending(
        server,
        &it.run,
        tailer::pending_questions(&it.run).unwrap_or_default(),
    );
    if !pending
        .iter()
        .any(|q| it.questions.first().is_some_and(|asked| asked.id == q.id))
    {
        return fail(false, "the question is no longer pending in this session");
    }
    let text = read(rt);
    if editor(&text).is_none() {
        if queued_count(&text).is_none() || !text.contains("⇧← to answer") {
            return fail(
                false,
                "the queued question is no longer available with a known answer key",
            );
        }
        if !key(server, rt, &it.pane, "shift+left").await {
            return fail(false, "pane exited");
        }
        let deadline = Instant::now() + WAIT;
        while editor(&read(rt)).is_none() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let Some(mut view) = editor(&read(rt)) else {
        return fail(false, "Codex did not open its question editor");
    };
    let source = pending
        .iter()
        .find(|q| q.id == it.questions[0].id)
        .expect("pending question checked above");
    let title = norm(&source.title);
    let position = pending.iter().position(|q| q.id == source.id).unwrap() + 1;
    if view.total != pending.len() {
        return fail(false, "the question queue changed; answer it in the pane");
    }
    if !view.footer.contains("⏎ submit") && !view.footer.contains("enter submit") {
        return fail(false, "Codex uses an unknown submit key");
    }
    // Same text in two pending requests cannot establish which native question is on screen.
    let duplicates = pending.iter().filter(|q| norm(&q.title) == title).count();
    if duplicates > 1 {
        return fail(false, "several pending questions have the same text");
    }
    let mut direction = "shift+left";
    for _ in 0..(view.total * 2) {
        if view.title == title && view.index == position {
            break;
        }
        if direction == "shift+left" && !view.footer.contains("⇧← next question") {
            direction = "shift+right";
        }
        if direction == "shift+right" && !view.footer.contains("⇧→ prev question") {
            return fail(
                false,
                "the requested question is not visible with known navigation keys",
            );
        }
        let before = view.clone();
        if !key(server, rt, &it.pane, direction).await {
            return fail(false, "pane exited");
        }
        let deadline = Instant::now() + WAIT;
        loop {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(next) = editor(&read(rt))
                && next.index != before.index
            {
                if next.total != pending.len() {
                    return fail(false, "the question queue changed during navigation");
                }
                view = next;
                break;
            }
            if Instant::now() >= deadline {
                return fail(false, "question navigation did not finish");
            }
        }
    }
    if view.title != title || view.index != position {
        return fail(false, "the requested question is not visible");
    }
    let q = &it.questions[0];
    let want: Vec<_> = q.options.iter().map(|o| norm(&o.label)).collect();
    if !want.is_empty() {
        // Every displayed option must match the card, in order. Clipping and custom drafts fail.
        if view.rows.len() != want.len() + 1
            || view.rows[..want.len()] != want
            || ![OTHER, "Other", INPUT].contains(&view.rows[want.len()].as_str())
        {
            return fail(
                false,
                "the visible options changed or an answer draft is already present",
            );
        }
        let target = if free {
            want.len()
        } else {
            let Some(index) = want.iter().position(|s| s == &norm(value)) else {
                return fail(false, "the answer is not an option");
            };
            index
        };
        for _ in 0..view.rows.len() {
            let Some(at) = view.pointer else {
                return fail(touched, "the selection is not visible");
            };
            if at == target {
                break;
            }
            if !key(
                server,
                rt,
                &it.pane,
                if at < target { "down" } else { "up" },
            )
            .await
            {
                return fail(touched, "pane exited");
            }
            let deadline = Instant::now() + WAIT;
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if let Some(next) = editor(&read(rt))
                    && next.pointer != Some(at)
                {
                    if next.title != title || next.rows[..want.len().min(next.rows.len())] != want {
                        return fail(touched, "the question changed during navigation");
                    }
                    view = next;
                    break;
                }
                if Instant::now() >= deadline {
                    return fail(touched, "the selection did not move");
                }
            }
        }
        if view.pointer != Some(target) {
            return fail(touched, "the answer is not selected");
        }
    } else if !free || view.input.trim_start_matches('›').trim() != INPUT {
        return fail(false, "an answer draft is already present");
    }
    if !current_run() || editor(&read(rt)).as_ref() != Some(&view) {
        return fail(false, "the question changed before typing the answer");
    }
    if free {
        let width = rt
            .screen
            .lock()
            .unwrap()
            .engine
            .cols()
            .saturating_sub(10)
            .max(1) as usize;
        let rows: usize = value
            .lines()
            .map(|l| {
                unicode_width::UnicodeWidthStr::width(l)
                    .max(1)
                    .div_ceil(width)
            })
            .sum();
        if rows > 8 {
            return fail(false, "this answer will not fit in the question editor");
        }
        let modes = crate::render::input_modes(server, &it.pane);
        if !modes.bracketed_paste {
            return fail(false, "this terminal cannot paste the answer safely");
        }
        let bytes = vk_term::encode::encode_paste(value, &modes);
        if rt.input(server.next_internal_input_id(), bytes).await
            == vk_proto::holder::InputStatus::ChildExited
        {
            return fail(false, "pane exited");
        }
        touched = true;
        let deadline = Instant::now() + WAIT;
        // Verify the same question, its navigation position and the exact editor contents.
        // A matching word elsewhere in scrollback is never evidence that the paste arrived.
        loop {
            if let Some(next) = editor(&read(rt)) {
                if next.title != title || next.index != view.index || next.total != view.total {
                    return fail(touched, "the question changed while pasting");
                }
                let echoed = if want.is_empty() {
                    next.rows.is_empty() && next.input.trim_start_matches('›').trim() == norm(value)
                } else {
                    next.rows.len() == want.len() + 1
                        && next.pointer == Some(want.len())
                        && next.rows[..want.len()] == want
                        && next.rows[want.len()] == norm(value)
                };
                if echoed {
                    view = next;
                    break;
                }
            }
            if Instant::now() >= deadline {
                return fail(touched, "the pasted answer is not fully visible");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    if !current_run() || editor(&read(rt)).as_ref() != Some(&view) {
        return fail(touched, "the question changed before submission");
    }
    if !key(server, rt, &it.pane, "enter").await {
        return fail(touched, "pane exited");
    }
    let deadline = Instant::now() + Duration::from_secs(7);
    let mut gone = None;
    loop {
        tokio::time::sleep(Duration::from_millis(60)).await;
        if let Some(recorded) = tailer::question_answer(&it.run, &q.id) {
            if recorded.trim() == value.trim() {
                return (DeliveryState::Delivered, None);
            }
            return fail(true, "a different answer was recorded by another client");
        }
        let text = read(rt);
        let accepted = editor(&text).is_some_and(|e| e.total < view.total)
            || queued_count(&text).is_some_and(|n| n < view.total);
        if accepted {
            return (DeliveryState::DecisionRecorded, None);
        }
        let main = !has_questions(&text) && text.contains("Ask Codex to");
        if main {
            if gone.get_or_insert_with(Instant::now).elapsed() >= Duration::from_millis(250) {
                return (DeliveryState::DecisionRecorded, None);
            }
        } else {
            gone = None;
        }
        if text.contains("Expand terminal to read the entire option") {
            return fail(
                true,
                "Codex did not accept the selected option; check the pane",
            );
        }
        if Instant::now() >= deadline {
            return fail(true, "Codex has not confirmed the answer");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUEUED: &str = "• Working (27m 25s • esc to interrupt)\n\n• Queued follow-up inputs\n  ? 1 question\n    ⇧← to answer\n\n› Ask Codex to do anything\n";
    const CHOICE: &str = "• Queued follow-up inputs\n\n  Which environment should receive\n  the preview?\n\n  › 1. Test\n    2. Production\n    3. Other\n\n  enter submit   ⌃] skip   ⇧→ main prompt\n";
    const FREE: &str = "• Queued follow-up inputs\n\n  1 of 2\n\n  Any constraints?\n\n  Type your answer\n\n  enter submit   ⌃] skip   ⇧→ main prompt   ⇧← next question\n";

    #[test]
    fn queued_questions_are_distinct_from_queued_messages() {
        assert_eq!(queued_count(QUEUED), Some(1));
        assert_eq!(
            queued_count(&QUEUED.replace("1 question", "3 questions")),
            Some(3)
        );
        assert_eq!(
            queued_count("• Queued follow-up inputs\n  ↳ Please keep going\n›"),
            None
        );
        assert_eq!(queued_count("? 1 question\n›"), None);
        assert!(editor(QUEUED).is_none());
        let steers = QUEUED.replace("  ? 1 question", "  ↳ A queued steer\n    that wraps\n  ↳ Another steer\n    on two lines\n  ? 1 question (20s)");
        assert_eq!(queued_count(&steers), Some(1));
    }

    #[test]
    fn single_choice_and_multiple_free_text_questions_follow_codex_layout() {
        let choice = editor(CHOICE).unwrap();
        assert_eq!((choice.index, choice.total), (1, 1));
        assert_eq!(
            choice.title,
            "Which environment should receive the preview?"
        );
        assert_eq!(choice.rows, ["Test", "Production", "Other"]);
        assert_eq!(choice.pointer, Some(0));
        let free = editor(FREE).unwrap();
        assert_eq!((free.index, free.total), (1, 2));
        assert!(free.rows.is_empty());
        assert_eq!(free.input, INPUT);
        assert_eq!(
            editor(&FREE.replace(INPUT, "Keep the service running"))
                .unwrap()
                .input,
            "Keep the service running"
        );
    }

    #[test]
    fn wrapped_choices_and_inline_other_drafts_keep_their_full_text() {
        let text = CHOICE
            .replace("› 1. Test", "  1. Test")
            .replace("  3. Other", "› 3. Keep the service\n       running");
        let view = editor(&text).unwrap();
        assert_eq!(view.pointer, Some(2));
        assert_eq!(view.rows[2], "Keep the service running");
        assert!(
            editor(&CHOICE.replace(
                "enter submit   ⌃] skip",
                "Expand terminal to read the entire option"
            ))
            .is_none()
        );
        assert!(editor(&CHOICE.replace("›", " ")).is_none());
        assert!(editor(&CHOICE.replace("2. Production", "4. Production")).is_none());
    }
}
