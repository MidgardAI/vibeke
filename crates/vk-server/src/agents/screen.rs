//! Screen detection fallback (04 §9) for Claude and Codex, and key planning for verified
//! keystroke delivery (§8). Screen matches only add information; they never override a healthy
//! structured transport (§2.5 rule 3).

use super::Harness;
use regex::Regex;
use std::sync::LazyLock;
use vk_agents::manifest::{HoldTracker, Snapshot};
use vk_proto::model::*;

#[derive(Debug, Clone)]
pub struct Dialog {
    pub kind: InteractionKind,
    pub title: String,
    pub tool: Option<String>,
    pub command: Option<String>,
    /// (digit, label, accelerator key if shown like "(y)")
    pub options: Vec<(u8, String, Option<char>)>,
    pub pointer: Option<u8>,
    pub fingerprint: String,
    pub confidence: f32,
    /// Manifest screen rule that matched (manifest-driven harnesses; empty for code-backed).
    pub rule: String,
}

impl Dialog {
    pub fn options_as_question(&self) -> Vec<Question> {
        if self.kind != InteractionKind::Question {
            return vec![];
        }
        vec![Question {
            id: "q0".into(),
            prompt: self.title.clone(),
            header: None,
            multi: false,
            options: self
                .options
                .iter()
                .map(|(n, l, _)| QuestionOption {
                    id: n.to_string(),
                    label: l.clone(),
                    description: None,
                    selected: false,
                })
                .collect(),
            allow_free_text: false,
        }]
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScreenMatch {
    pub state: Option<(Execution, f32)>,
    pub dialog: Option<Dialog>,
}

static OPTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[\s│┃|]*(?P<ptr>[❯›>▶])?\s*(?P<n>[1-9])[.)]\s+(?P<label>.+?)\s*[│┃|]?\s*$")
        .unwrap()
});
static ACCEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((?P<k>[a-z])\)\s*$").unwrap());
static QUESTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(do you want to (proceed|make this edit|create|run|allow|fetch)[^?]*\?|would you like to (run|make|apply|allow)[^?]*\?|allow (this|command|the following)[^?]*\?|approve[^?]*\?|proceed\?)").unwrap()
});

/// Evaluate the bottom of the visible screen. Code-backed harnesses (and custom wrappers of
/// them) use the built-in evaluator; everything else runs its manifest's rules (04 §9).
pub fn evaluate(h: Harness, screen: &str) -> ScreenMatch {
    evaluate_snapshot(h, &Snapshot::from_text(screen), 0, None).0
}

/// [`evaluate`] with the full terminal context (cell styles, title, cursor, alternate screen,
/// OSC 133) and the pane's [`HoldTracker`] for `hold_ms` rules. The second value is the
/// milliseconds after which the screen should be evaluated again (a `hold_ms` rule that matches
/// but has not held long enough yet).
pub fn evaluate_snapshot(
    h: Harness,
    snap: &Snapshot,
    now_ms: i64,
    hold: Option<&mut HoldTracker>,
) -> (ScreenMatch, Option<u64>) {
    let base = h.base();
    if !matches!(
        base,
        Harness::Claude | Harness::Codex | Harness::Pi | Harness::Omp
    ) {
        return evaluate_manifest(base, snap, now_ms, hold);
    }
    let mut m = evaluate_code(base, snap);
    // 04 §9.2: a boxed, numbered, pointer-marked list that matches no rule is a provisional
    // question rather than a silent `idle`.
    let heuristic = base.manifest().is_none_or(|l| l.m.screen.unknown_dialog);
    if heuristic && m.dialog.is_none() && m.state.as_ref().is_none_or(|s| s.0 == Execution::Idle) {
        let lines: Vec<&str> = snap.lines.iter().map(String::as_str).collect();
        let start = lines.len().saturating_sub(40);
        if let Some(d) = vk_agents::manifest::unknown_dialog_in(&lines[start..]) {
            m.state = None;
            m.dialog = Some(dialog_of(d));
        }
    }
    (m, None)
}

fn evaluate_code(h: Harness, snap: &Snapshot) -> ScreenMatch {
    let lines: Vec<&str> = snap.lines.iter().map(String::as_str).collect();
    let start = lines.len().saturating_sub(40);
    let tail = &lines[start..];
    let mut m = ScreenMatch::default();
    let text = tail.join("\n");
    let working = match h {
        Harness::Claude | Harness::Pi | Harness::Omp => {
            text.contains("esc to interrupt")
                || text.contains("Esc to interrupt")
                || text.contains("Working...")
        }
        _ => {
            text.contains("esc to interrupt")
                || text.contains("Esc to interrupt")
                || tail
                    .iter()
                    .any(|l| l.trim_start().starts_with("• Working") || l.contains("Working ("))
        }
    };
    // Dialog: a question line followed by numbered options.
    if let Some(qi) = tail.iter().rposition(|l| QUESTION.is_match(l)) {
        let mut options = Vec::new();
        let mut pointer = None;
        for l in &tail[qi + 1..] {
            if let Some(c) = OPTION.captures(l) {
                let n: u8 = c["n"].parse().unwrap_or(0);
                let label = c["label"].trim().to_string();
                let accel = ACCEL.captures(&label).and_then(|a| a["k"].chars().next());
                if c.name("ptr").is_some() {
                    pointer = Some(n);
                }
                options.push((n, label, accel));
            } else if !options.is_empty()
                && !l.trim().is_empty()
                && !l.contains('│')
                && !l.contains('╰')
            {
                break;
            }
        }
        if options.len() >= 2 {
            let title = QUESTION
                .find(tail[qi])
                .map(|x| x.as_str().to_string())
                .unwrap_or_else(|| tail[qi].trim().to_string());
            // The command sits in the box above the question.
            let above: Vec<String> = tail[..qi]
                .iter()
                .rev()
                .take(8)
                .map(|l| {
                    l.trim()
                        .trim_matches(|c| c == '│' || c == '┃')
                        .trim()
                        .to_string()
                })
                .take_while(|l| !l.starts_with('╭') && !l.starts_with('─'))
                .filter(|l| !l.is_empty())
                .collect();
            let mut above: Vec<String> = above.into_iter().rev().collect();
            let tool = above
                .first()
                .filter(|l| {
                    l.ends_with("command")
                        || l.starts_with("Edit")
                        || l.starts_with("Write")
                        || l.contains("file")
                })
                .cloned();
            if tool.is_some() {
                above.remove(0);
            }
            let command = above.first().cloned();
            let kind = if options.iter().any(|(_, l, _)| l.starts_with("Yes")) {
                InteractionKind::Approval
            } else {
                InteractionKind::Question
            };
            let fingerprint = format!(
                "{:x}",
                fnv(&format!(
                    "{title}|{}|{}",
                    command.clone().unwrap_or_default(),
                    options
                        .iter()
                        .map(|o| o.1.as_str())
                        .collect::<Vec<_>>()
                        .join("|")
                ))
            );
            m.dialog = Some(Dialog {
                kind,
                title,
                tool: tool.map(|t| {
                    if t.contains("Bash") || t.ends_with("command") {
                        "Bash".into()
                    } else {
                        t
                    }
                }),
                command,
                options,
                pointer,
                fingerprint,
                confidence: 0.9,
                rule: String::new(),
            });
        }
    }
    m.state = if m.dialog.is_some() {
        None
    } else if working {
        Some((Execution::Working, 0.8))
    } else if tail.iter().any(|l| {
        l.trim_start().starts_with('>') || l.contains("│ >") || l.trim_start().starts_with('›')
    }) {
        Some((Execution::Idle, 0.6))
    } else {
        None
    };
    m
}

/// The manifest whose screen rules apply to `h` (its own, else `[screen] manifest = "<id>"`).
pub fn screen_manifest(h: Harness) -> Option<std::sync::Arc<vk_agents::manifest::Loaded>> {
    let l = h.manifest()?;
    if l.has_screen_rules() || l.m.screen.manifest.is_empty() {
        return Some(l);
    }
    super::manifests::lookup(&l.m.screen.manifest).map(|(_, l)| l)
}

fn col_of(c: vk_proto::render::Color) -> vk_agents::manifest::Col {
    use vk_agents::manifest::Col;
    match c {
        vk_proto::render::Color::Default => Col::Default,
        vk_proto::render::Color::Indexed(i) => Col::Indexed(i),
        vk_proto::render::Color::Rgb(r, g, b) => Col::Rgb(r, g, b),
    }
}

/// What the screen engine sees of a pane, for the manifest DSL's context matchers (04 §9.1):
/// rows (as `screen_text` joins them), per-cell styles, the OSC 0/2 title, the cursor, the
/// alternate screen and the shell-integration state at the cursor (`A` prompt row, `B` command
/// line continuation, `C` a command running).
pub fn snapshot_of(engine: &vk_term::Engine) -> Snapshot {
    use vk_agents::manifest::CellStyle;
    use vk_proto::render::{attr, mark};
    let text = engine.screen_text();
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let rows = engine.visible_rows();
    let mut styles: Vec<Vec<CellStyle>> = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        let mut cells = Vec::with_capacity(line.chars().count());
        if let Some(row) = rows.get(i) {
            for span in &row.spans {
                let st = CellStyle {
                    fg: col_of(span.style.fg),
                    bg: col_of(span.style.bg),
                    bold: span.style.attrs & attr::BOLD != 0,
                    dim: span.style.attrs & attr::DIM != 0,
                    inverse: span.style.attrs & attr::INVERSE != 0,
                    underline: span.style.attrs & attr::ANY_UNDERLINE != 0,
                };
                cells.extend(span.text.chars().map(|_| st));
            }
        }
        cells.truncate(line.chars().count());
        styles.push(cells);
    }
    let cur = engine.cursor();
    let alt = engine.modes().alt_screen;
    let osc133 = if alt {
        None
    } else {
        match rows.get(cur.row as usize).map(|r| r.mark) {
            Some(mark::PROMPT) => Some('A'),
            Some(mark::PROMPT_CONT) => Some('B'),
            _ => engine.last_command().filter(|c| c.running).map(|_| 'C'),
        }
    };
    Snapshot {
        lines,
        styles,
        alt_screen: alt,
        title: engine.title(),
        cursor: Some((cur.row as usize, cur.col as usize)),
        osc133,
    }
}

/// A manifest dialog match as the server's [`Dialog`].
fn dialog_of(d: vk_agents::manifest::DialogMatch) -> Dialog {
    let kind = match d.kind.as_str() {
        "question" => InteractionKind::Question,
        "plan_review" => InteractionKind::PlanReview,
        _ => InteractionKind::Approval,
    };
    let fingerprint = format!(
        "{:x}",
        fnv(&format!(
            "{}|{}|{}",
            d.title,
            d.command.clone().unwrap_or_default(),
            d.options
                .iter()
                .map(|o| o.1.as_str())
                .collect::<Vec<_>>()
                .join("|")
        ))
    );
    Dialog {
        kind,
        title: d.title,
        tool: d.command.as_ref().map(|_| "Bash".to_string()),
        command: d.command,
        options: d.options,
        pointer: d.pointer,
        fingerprint,
        confidence: d.confidence,
        rule: d.rule_id,
    }
}

impl Dialog {
    /// Opened by the unknown-dialog heuristic: shown, never answered by keystrokes.
    pub fn is_unknown(&self) -> bool {
        self.rule == vk_agents::manifest::UNKNOWN_DIALOG_RULE
    }
}

fn evaluate_manifest(
    h: Harness,
    snap: &Snapshot,
    now_ms: i64,
    hold: Option<&mut HoldTracker>,
) -> (ScreenMatch, Option<u64>) {
    let Some(l) = screen_manifest(h) else {
        return (ScreenMatch::default(), None);
    };
    let r = l.evaluate_snapshot(snap, now_ms, hold);
    let pending = r.hold_pending_ms;
    let mut m = ScreenMatch {
        state: r
            .state
            .and_then(|(s, c, _)| Execution::parse(&s).map(|e| (e, c))),
        dialog: None,
    };
    if let Some(d) = r.dialog {
        m.state = None;
        m.dialog = Some(dialog_of(d));
    }
    (m, pending)
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Keys that select the option matching `answer` (accelerators preferred: atomic, 04 §8 step 3).
pub fn keys_for(h: Harness, d: &Dialog, it: &Interaction, answer: &Answer) -> Option<Vec<String>> {
    let h = h.base();
    if !matches!(
        h,
        Harness::Claude | Harness::Codex | Harness::Pi | Harness::Omp
    ) {
        use vk_agents::manifest::{DialogMatch, KeyIntent, plan_keys};
        let l = screen_manifest(h)?;
        let spec = l.dialog_spec(&d.rule).cloned().unwrap_or_default();
        let intent = match (it.kind, answer.decision) {
            (_, Some(Decision::Cancel)) => return None,
            (InteractionKind::Question, _) => {
                KeyIntent::Option(answer.choices.first().and_then(|(_, o)| o.first())?.clone())
            }
            (_, Some(Decision::Allow)) => KeyIntent::Allow,
            (_, Some(Decision::AllowAlways)) => KeyIntent::AllowAlways,
            (_, Some(Decision::Deny)) | (_, None) => KeyIntent::Deny,
        };
        let dm = DialogMatch {
            rule_id: d.rule.clone(),
            kind: it.kind.as_str().to_string(),
            title: d.title.clone(),
            command: d.command.clone(),
            options: d.options.clone(),
            pointer: d.pointer,
            confidence: d.confidence,
        };
        return plan_keys(&dm, &spec, &intent);
    }
    let pick = |pred: &dyn Fn(&str) -> bool| {
        d.options
            .iter()
            .find(|(_, l, _)| pred(&l.to_lowercase()))
            .cloned()
    };
    let opt = match (it.kind, answer.decision) {
        (_, Some(Decision::Cancel)) => return None,
        (InteractionKind::Question, _) => {
            let want = answer.choices.first().and_then(|(_, o)| o.first())?;
            d.options
                .iter()
                .find(|(n, l, _)| {
                    n.to_string() == *want || l == want || l.starts_with(want.as_str())
                })
                .cloned()
        }
        (_, Some(Decision::Allow)) => {
            pick(&|l| l.starts_with("yes") && !l.contains("don't ask") && !l.contains("always"))
        }
        (_, Some(Decision::AllowAlways)) => {
            pick(&|l| l.starts_with("yes") && (l.contains("don't ask") || l.contains("always")))
        }
        (_, Some(Decision::Deny)) | (_, None) => pick(&|l| l.starts_with("no")),
    }?;
    let (n, _, accel) = opt;
    Some(match (h, accel) {
        (Harness::Codex, Some(k)) => vec![k.to_string()],
        // Claude selects and confirms on the digit.
        (Harness::Claude, _) => vec![n.to_string()],
        // pi/omp generic select dialogs: arrows to the target row, then enter (best effort).
        (Harness::Pi | Harness::Omp, _) => {
            let delta = n as i32 - d.pointer.unwrap_or(1) as i32;
            let key = if delta >= 0 { "down" } else { "up" };
            let mut k: Vec<String> = (0..delta.unsigned_abs()).map(|_| key.to_string()).collect();
            k.push("enter".into());
            k
        }
        (_, None) => vec![n.to_string(), "enter".into()],
        (_, Some(k)) => vec![k.to_string()],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_DIALOG: &str = "╭──────────────────────────────────────────────╮
│ Bash command                                 │
│                                              │
│   rm -rf build                               │
│   Remove build output                        │
│                                              │
│ Do you want to proceed?                      │
│ ❯ 1. Yes                                     │
│   2. Yes, and don't ask again for rm commands│
│   3. No, and tell Claude what to do differently (esc) │
╰──────────────────────────────────────────────╯";

    #[test]
    fn claude_approval_dialog_and_keys() {
        let m = evaluate(Harness::Claude, CLAUDE_DIALOG);
        let d = m.dialog.expect("dialog");
        assert_eq!(d.kind, InteractionKind::Approval);
        assert_eq!(d.options.len(), 3);
        assert_eq!(d.pointer, Some(1));
        assert_eq!(d.command.as_deref(), Some("rm -rf build"));
        let it = Interaction {
            kind: InteractionKind::Approval,
            ..super::super::harness_tests_blank()
        };
        let deny = Answer {
            decision: Some(Decision::Deny),
            ..Default::default()
        };
        assert_eq!(
            keys_for(Harness::Claude, &d, &it, &deny),
            Some(vec!["3".to_string()])
        );
        let always = Answer {
            decision: Some(Decision::AllowAlways),
            ..Default::default()
        };
        assert_eq!(
            keys_for(Harness::Claude, &d, &it, &always),
            Some(vec!["2".to_string()])
        );
    }

    #[test]
    fn working_and_idle() {
        assert_eq!(
            evaluate(Harness::Claude, "✻ Thinking… (3s · esc to interrupt)")
                .state
                .unwrap()
                .0,
            Execution::Working
        );
        assert_eq!(
            evaluate(Harness::Codex, "• Working (5s • esc to interrupt)")
                .state
                .unwrap()
                .0,
            Execution::Working
        );
        assert!(evaluate(Harness::Claude, "hello").state.is_none());
    }
}
