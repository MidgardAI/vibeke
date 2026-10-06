//! Screen detection fallback (04 §9) for Claude and Codex, and key planning for verified
//! keystroke delivery (§8). Screen matches only add information; they never override a healthy
//! structured transport (§2.5 rule 3).

use super::Harness;
use regex::Regex;
use std::sync::LazyLock;
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

/// Evaluate the bottom of the visible screen.
pub fn evaluate(h: Harness, screen: &str) -> ScreenMatch {
    let lines: Vec<&str> = screen.lines().collect();
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
        Harness::Codex => {
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
    let pick = |pred: &dyn Fn(&str) -> bool| {
        d.options
            .iter()
            .find(|(_, l, _)| pred(&l.to_lowercase()))
            .cloned()
    };
    let opt = match (it.kind, answer.decision) {
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
        (Harness::Codex, None) => vec![n.to_string(), "enter".into()],
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
