//! Claude's `AskUserQuestion` dialog on screen (04 §6.1.2, §8): one tab per question, the
//! visible question with its numbered options, and the **Submit** review after the last tab.
//!
//! ```text
//! ←  ☐ Scope  ☒ Publisher  ☐ Existing Qs  ✔ Submit  →
//!
//! Which hub resources should be shareable through a public link?
//!
//! ❯ 1. Playable materials (Recommended)
//!      Podcasts, videos and documents.
//!   2. Podcasts only
//!   3. Type something.
//! ────────────────────────────────────────
//!   4. Chat about this
//!
//! Enter to select · Tab/Arrow keys to navigate · Esc to cancel
//! ```
//!
//! A single question shows one tab without arrows or **Submit**, and Claude submits as soon as
//! an option is selected. With several questions, selecting an option moves to the next tab;
//! after the last one the review lists the answers above `1. Submit answers  2. Cancel`.
//!
//! The parser reads only the bottom of the screen and fails closed: anything it does not fully
//! recognise is not this dialog. Answering it is planned here one verified step at a time
//! ([`next_step`]); `screen_walk::walk_ask` sends the keys.

use super::screen_picker::fnv;
use regex::Regex;
use std::sync::LazyLock;
use vk_proto::model::*;

pub const RULE_ID: &str = "claude:ask_user_question";
pub const REVIEW_TITLE: &str = "Review your answers";
const READY: &str = "Ready to submit your answers?";
const SUBMIT: &str = "Submit answers";
const CANCEL: &str = "Cancel";
const CHAT: &str = "Chat about this";
const FREE_TEXT: &str = "Type something";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskTab {
    /// The question's header (`Scope`); empty when the row is too narrow to show it.
    pub header: String,
    /// The tab shows a checked box: the question has an answer in the dialog.
    pub answered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskRow {
    /// The number Claude shows (`1.`); a display index, never pressed.
    pub n: u8,
    pub label: String,
    pub description: Option<String>,
    /// Checkbox state of a multi-select row.
    pub checked: Option<bool>,
}

impl AskRow {
    /// The free-text row (`Type something.`): selecting it opens a text field.
    pub fn is_free_text(&self) -> bool {
        self.label.starts_with(FREE_TEXT)
    }

    /// `Chat about this`: leaves the dialog and returns to the conversation.
    pub fn is_chat(&self) -> bool {
        self.label == CHAT
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskView {
    Question,
    Review,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskScreen {
    /// One tab per question, in order (left to right).
    pub tabs: Vec<AskTab>,
    /// The tab row ends in `✔ Submit` (several questions).
    pub submit_tab: bool,
    pub view: AskView,
    /// The visible question (wrapped lines joined), or [`REVIEW_TITLE`].
    pub title: String,
    /// The review's list of answers (joined); empty for a question.
    pub body: String,
    pub rows: Vec<AskRow>,
    /// Index into `rows` of the pointed row.
    pub pointer: Option<usize>,
}

impl AskScreen {
    /// A multi-select question (checkbox rows).
    pub fn multi(&self) -> bool {
        self.rows.iter().any(|r| r.checked.is_some())
    }

    /// The rows that are answers (not the free-text field or `Chat about this`).
    pub fn options(&self) -> impl Iterator<Item = &AskRow> {
        self.rows
            .iter()
            .filter(|r| !r.is_free_text() && !r.is_chat())
    }

    /// What the screen asks: changes when the dialog moves to another question or the review.
    pub fn fingerprint(&self) -> String {
        let rows = self
            .rows
            .iter()
            .map(|r| r.label.as_str())
            .collect::<Vec<_>>()
            .join("|");
        let view = match self.view {
            AskView::Question => "q",
            AskView::Review => "r",
        };
        format!("{:x}", fnv(&format!("ask|{view}|{}|{rows}", self.title)))
    }

    /// The navigation state a step must change.
    pub fn state(&self) -> (AskView, String, Option<usize>) {
        (self.view, self.title.clone(), self.pointer)
    }
}

static ROW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<lead>\s*(?P<ptr>❯)?\s*)(?P<n>\d{1,2})\.\s+(?P<label>\S.*?)\s*$").unwrap()
});
static CHECK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\[[ xX✔✓]\]|☐|☒|☑|◯|◉)\s+").unwrap());

fn indent(l: &str) -> usize {
    l.chars().take_while(|c| c.is_whitespace()).count()
}

fn is_rule(l: &str) -> bool {
    let t = l.trim();
    t.chars().count() >= 10 && t.chars().all(|c| c == '─' || c == '━')
}

const TAB_BOXES: &[char] = &['☐', '☒', '☑'];

/// `←  ☐ Scope  ☒ Publisher  ✔ Submit  →`, or ` ☐ Scope ` for a single question.
pub fn tab_row(l: &str) -> Option<(Vec<AskTab>, bool)> {
    let t = l.trim();
    let t = t.strip_prefix('←').unwrap_or(t);
    let t = t.strip_suffix('→').unwrap_or(t).trim();
    if !t.starts_with(TAB_BOXES) {
        return None;
    }
    let mut segs: Vec<(char, String)> = Vec::new();
    for c in t.chars() {
        if TAB_BOXES.contains(&c) || c == '✔' {
            segs.push((c, String::new()));
        } else if let Some(s) = segs.last_mut() {
            s.1.push(c);
        }
    }
    let mut tabs = Vec::new();
    let mut submit = false;
    let n = segs.len();
    for (i, (glyph, text)) in segs.into_iter().enumerate() {
        let text = text.trim();
        let text = text.strip_prefix("answered ").unwrap_or(text).trim();
        if glyph == '✔' {
            // Only the last tab is `✔ Submit`.
            if i + 1 != n || text != "Submit" {
                return None;
            }
            submit = true;
            continue;
        }
        tabs.push(AskTab {
            header: text.to_string(),
            answered: glyph != '☐',
        });
    }
    if tabs.is_empty() || (tabs.len() > 1 && !submit) {
        return None;
    }
    Some((tabs, submit))
}

/// The rows of a numbered list starting at `lines[0]`. Continuation lines (deeper than the
/// number) are descriptions; rules and blank lines may sit between rows. Anything else ends
/// the parse with `None` (fail closed). Returns the rows, the pointed row, and how many lines
/// were consumed.
fn parse_rows(lines: &[&str]) -> Option<(Vec<AskRow>, Option<usize>, usize)> {
    let first = ROW.captures(lines.first()?)?;
    let col = first["lead"].chars().count();
    let mut out: Vec<AskRow> = Vec::new();
    let mut pointer = None;
    let mut used = 0;
    for l in lines {
        if l.trim().is_empty() || is_rule(l) {
            used += 1;
            continue;
        }
        match ROW.captures(l) {
            Some(c) if c["lead"].chars().count() == col => {
                let n: u8 = c["n"].parse().ok()?;
                if c.name("ptr").is_some() {
                    if pointer.is_some() {
                        return None;
                    }
                    pointer = Some(out.len());
                }
                let mut label = c["label"].trim().to_string();
                let mut checked = None;
                if let Some(m) = CHECK.captures(&label) {
                    let end = m.get(0).map_or(0, |x| x.end());
                    checked = Some(!matches!(&m[1], "[ ]" | "☐" | "◯"));
                    label = label[end..].trim().to_string();
                }
                // A `✔` after the label marks the answer already chosen for this question.
                if let Some(x) = label.strip_suffix('✔') {
                    label = x.trim().to_string();
                }
                out.push(AskRow {
                    n,
                    label,
                    description: None,
                    checked,
                });
            }
            _ if indent(l) > col && !out.is_empty() => {
                let prev = out.last_mut()?;
                let more = l.trim().to_string();
                prev.description = Some(match prev.description.take() {
                    Some(d) => format!("{d} {more}"),
                    None => more,
                });
            }
            _ => break,
        }
        used += 1;
    }
    // Trailing blanks and rules are not part of the list.
    while used > 0 && (lines[used - 1].trim().is_empty() || is_rule(lines[used - 1])) {
        used -= 1;
    }
    Some((out, pointer, used))
}

/// The bottom of the screen: the last 40 lines down to the last non-blank one.
fn bottom<'a>(lines: &'a [&'a str]) -> &'a [&'a str] {
    let mut end = lines.len();
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    &lines[end.saturating_sub(40)..end]
}

fn is_footer(l: &str) -> bool {
    let t = l.trim();
    t.contains("Enter to select") && t.contains("navigate") && t.contains("Esc to cancel")
}

/// Claude's `AskUserQuestion` dialog at the bottom of the screen, if it shows one.
pub fn detect(screen_lines: &[&str]) -> Option<AskScreen> {
    let lines = bottom(screen_lines);
    question_view(lines).or_else(|| review_view(lines))
}

fn question_view(lines: &[&str]) -> Option<AskScreen> {
    let last = lines.len().checked_sub(1)?;
    // The key hints may wrap onto a second line in a narrow pane.
    let f = if is_footer(lines[last]) {
        last
    } else if last > 0
        && !lines[last - 1].trim().is_empty()
        && is_footer(&format!(
            "{} {}",
            lines[last - 1].trim(),
            lines[last].trim()
        ))
    {
        last - 1
    } else {
        return None;
    };
    // The nearest tab row above the footer starts the dialog.
    let t = (0..f)
        .rev()
        .take(36)
        .find(|&i| tab_row(lines[i]).is_some())?;
    let (tabs, submit_tab) = tab_row(lines[t])?;
    let region = &lines[t + 1..f];
    let first = region
        .iter()
        .position(|l| ROW.is_match(l) && !l.trim().is_empty())?;
    let title = region[..first]
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        return None;
    }
    let (rows, pointer, used) = parse_rows(&region[first..])?;
    // Everything between the list and the footer must be part of the list.
    if region[first + used..].iter().any(|l| !l.trim().is_empty()) {
        return None;
    }
    if rows.len() < 2 {
        return None;
    }
    Some(AskScreen {
        tabs,
        submit_tab,
        view: AskView::Question,
        title,
        body: String::new(),
        rows,
        pointer,
    })
}

fn review_view(lines: &[&str]) -> Option<AskScreen> {
    let rv = lines.iter().rposition(|l| l.trim() == REVIEW_TITLE)?;
    let (tabs, submit_tab) = (rv.saturating_sub(3)..rv)
        .rev()
        .find_map(|i| tab_row(lines[i]))?;
    if !submit_tab {
        return None;
    }
    let after = &lines[rv + 1..];
    let ready = after.iter().position(|l| l.trim() == READY)?;
    let first = ready
        + 1
        + after[ready + 1..]
            .iter()
            .position(|l| !l.trim().is_empty())?;
    let (rows, pointer, used) = parse_rows(&after[first..])?;
    // After the choices only a key-hint line may follow.
    let rest: Vec<&&str> = after[first + used..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .collect();
    if rest.len() > 1
        || rest
            .iter()
            .any(|l| !(l.contains("Esc") || l.contains("Enter")))
    {
        return None;
    }
    let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
    if labels != [SUBMIT, CANCEL] {
        return None;
    }
    let body = after[..ready]
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Some(AskScreen {
        tabs,
        submit_tab,
        view: AskView::Review,
        title: REVIEW_TITLE.to_string(),
        body,
        rows,
        pointer,
    })
}

// ---- matching -------------------------------------------------------------------------------------

/// Text as a comparison key: case, whitespace (wrapping) and inline markdown marks ignored.
pub fn key(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '`' | '*' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// The index of the interaction's question the screen shows (by its text).
pub fn question_index(a: &AskScreen, it: &Interaction) -> Option<usize> {
    let shown = key(&a.title);
    if shown.is_empty() {
        return None;
    }
    let prompts: Vec<String> = it.questions.iter().map(|q| key(&q.prompt)).collect();
    let exact: Vec<usize> = (0..prompts.len())
        .filter(|&i| prompts[i] == shown)
        .collect();
    if exact.len() == 1 {
        return Some(exact[0]);
    }
    if !exact.is_empty() {
        return None;
    }
    // The screen may add a body under the question, or (rarely) cut it short.
    let near: Vec<usize> = (0..prompts.len())
        .filter(|&i| {
            let p = &prompts[i];
            !p.is_empty()
                && (shown.contains(p.as_str()) || (shown.len() >= 12 && p.contains(&shown)))
        })
        .collect();
    (near.len() == 1).then(|| near[0])
}

fn header_matches(shown: &str, want: &str) -> bool {
    let s = key(shown.trim_end_matches('…'));
    let w = key(want);
    s.is_empty() || w.starts_with(&s)
}

/// Does the dialog on screen belong to `it`? Its tabs must be the interaction's questions
/// (count and headers), and the visible question one of them, or the review of them.
pub fn matches(a: &AskScreen, it: &Interaction) -> bool {
    if it.kind != InteractionKind::Question || it.questions.is_empty() {
        return false;
    }
    // A screen-sourced interaction describes one view of the dialog (one question).
    let one_view = it.questions.len() == 1 && question_index(a, it).is_some();
    if !one_view {
        if a.tabs.len() != it.questions.len() {
            return false;
        }
        let headers_ok = a
            .tabs
            .iter()
            .zip(&it.questions)
            .all(|(t, q)| match &q.header {
                Some(h) => header_matches(&t.header, h),
                None => true,
            });
        if !headers_ok {
            return false;
        }
    }
    match a.view {
        AskView::Question => question_index(a, it).is_some(),
        AskView::Review => one_view || it.questions.len() > 1,
    }
}

// ---- answering ------------------------------------------------------------------------------------

/// The next step of answering the dialog, read off one verified screen read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskStep {
    /// Move (pointer or tab); the screen must change before planning again.
    Key(String),
    /// Select the pointed row. `question`: the interaction question this answers; `None` for
    /// **Submit answers** on the review. The dialog moves on (or closes).
    Select {
        key: String,
        question: Option<usize>,
    },
    Fail(String),
}

/// The label of the option the answer picks for each question (`None`: no choice given).
pub fn wanted(it: &Interaction, answer: &Answer) -> Vec<Option<String>> {
    it.questions
        .iter()
        .map(|q| {
            let want = answer
                .choices
                .iter()
                .find(|(id, _)| *id == q.id)
                .and_then(|(_, picked)| picked.first())?;
            Some(
                q.options
                    .iter()
                    .find(|o| o.id == *want)
                    .or_else(|| q.options.iter().find(|o| o.label == *want))
                    .map(|o| o.label.clone())
                    .unwrap_or_else(|| want.clone()),
            )
        })
        .collect()
}

/// The row showing `label`: the same text, else the same ignoring case and spacing, else a
/// unique prefix (a long label may wrap). Free text and `Chat about this` are never answers.
fn row_for(a: &AskScreen, label: &str) -> Option<usize> {
    let cands: Vec<usize> = (0..a.rows.len())
        .filter(|&i| !a.rows[i].is_free_text() && !a.rows[i].is_chat())
        .collect();
    let one = |m: Vec<usize>| (m.len() == 1).then(|| m[0]);
    let want = key(label);
    one(cands
        .iter()
        .copied()
        .filter(|&i| a.rows[i].label == label)
        .collect())
    .or_else(|| {
        one(cands
            .iter()
            .copied()
            .filter(|&i| key(&a.rows[i].label) == want)
            .collect())
    })
    .or_else(|| {
        one(cands
            .iter()
            .copied()
            .filter(|&i| {
                let have = key(&a.rows[i].label);
                !want.is_empty() && !have.is_empty() && want.starts_with(&have)
            })
            .collect())
    })
}

/// Move the pointer one row towards `target`, or select it.
fn toward(a: &AskScreen, target: usize, question: Option<usize>) -> AskStep {
    let Some(at) = a.pointer else {
        return AskStep::Fail("the dialog shows no pointed option".into());
    };
    if at == target {
        AskStep::Select {
            key: "enter".into(),
            question,
        }
    } else if target > at {
        AskStep::Key("down".into())
    } else {
        AskStep::Key("up".into())
    }
}

/// Plan one step. `want`: the label per question ([`wanted`]); `done`: the questions this walk
/// has already answered in the dialog.
pub fn next_step(
    a: &AskScreen,
    it: &Interaction,
    want: &[Option<String>],
    done: &[bool],
) -> AskStep {
    let pointer_on_text = a
        .pointer
        .and_then(|p| a.rows.get(p))
        .is_some_and(AskRow::is_free_text);
    // Switch tabs from tab `cur` to tab `to` (the review is the tab after the last question).
    let switch = |cur: usize, to: usize| -> AskStep {
        if pointer_on_text {
            // The free-text field takes ←/→ for its cursor: step off it first.
            return AskStep::Key("up".into());
        }
        AskStep::Key(if to < cur { "left" } else { "right" }.into())
    };
    let next_open = (0..want.len()).find(|&i| !done.get(i).copied().unwrap_or(false));
    match (a.view, question_index(a, it)) {
        (_, Some(k)) => {
            if done.get(k).copied().unwrap_or(false) {
                return match next_open {
                    Some(j) => switch(k, j),
                    None if a.submit_tab && it.questions.len() > 1 => switch(k, a.tabs.len()),
                    None => AskStep::Fail("the dialog did not move on after the answer".into()),
                };
            }
            if a.multi() {
                return AskStep::Fail(
                    "answer_in_pane: Vibeke cannot answer a multi-select question; answer it in the pane".into(),
                );
            }
            let Some(label) = want.get(k).cloned().flatten() else {
                return AskStep::Fail(format!(
                    "answer_in_pane: no choice for question {} of {}",
                    k + 1,
                    want.len()
                ));
            };
            match row_for(a, &label) {
                Some(t) => toward(a, t, Some(k)),
                None => AskStep::Fail(format!(
                    "answer_in_pane: \"{label}\" is not an option on screen (free-text answers are typed in the pane)"
                )),
            }
        }
        (AskView::Review, None) => {
            if let Some(j) = next_open {
                return switch(a.tabs.len(), j);
            }
            // The review lists the dialog's answers: they must be the ones just entered.
            let body = key(&a.body);
            if let Some(l) = want.iter().flatten().find(|l| !body.contains(&key(l))) {
                return AskStep::Fail(format!(
                    "the review does not show the answer \"{l}\"; check the pane"
                ));
            }
            match a.rows.iter().position(|r| r.label == SUBMIT) {
                Some(t) => toward(a, t, None),
                None => AskStep::Fail("the review offers no Submit answers".into()),
            }
        }
        (AskView::Question, None) => AskStep::Fail(
            "dialog_changed: the visible question is not part of this interaction".into(),
        ),
    }
}

/// Does answering `it` end on the review's **Submit answers** (several questions in one
/// dialog), rather than closing (or moving on) when its last question is selected?
pub fn needs_submit(a: &AskScreen, it: &Interaction) -> bool {
    a.submit_tab && it.questions.len() > 1
}
