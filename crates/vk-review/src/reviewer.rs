//! Optional reviewer runs (15 §6.1, §7, T4).
//!
//! A reviewer run is an ordinary agent run the user starts explicitly, with a prompt built
//! deterministically from the review package and shown in full before anything is launched.
//! Its findings are recorded as attributed review notes — agent opinions, never evidence. A
//! note cannot accept a review, waive a check or override an observation. An unassessed,
//! potentially blocking finding on the reviewed subject keeps the stronger readiness label
//! away until a user classifies it (blocking / not blocking / dismissed).

use serde::{Deserialize, Serialize};

use crate::{Actor, ActorKind, FieldHasher, new_id, truncate_utf8};

/// What the prompt is built from (copied out of the review package).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptInput {
    pub task_title: String,
    pub objective: String,
    pub constraints: Vec<String>,
    /// `(id, text, required, current status)`.
    pub criteria: Vec<(String, String, bool, String)>,
    pub stop_at: String,
    pub subject_id: String,
    pub subject_kind: String,
    pub base_sha: String,
    /// The commit holding the reviewed content (snapshot commit or head).
    pub content_sha: String,
    /// `(path, added, removed)`; `None` counts for binary files.
    pub files: Vec<(String, Option<u64>, Option<u64>)>,
    /// One line per recorded check run on this subject (`unit: passed`).
    pub checks: Vec<String>,
}

pub const PROMPT_VERSION: &str = "reviewer-v1";
/// Upper bound of a generated prompt.
pub const MAX_PROMPT_BYTES: usize = 12 * 1024;

/// The reviewable prompt (deterministic for the same input).
pub fn build_prompt(i: &PromptInput) -> String {
    let mut s = String::new();
    s.push_str(
        "You are reviewing another agent's work for a human. Review only; do not edit files, commit, push, \
         install anything or run commands that change state. Your findings are recorded as your opinion: \
         they cannot accept the review, waive checks or mark tests as passing.\n\n",
    );
    s.push_str(&format!("Task: {}\n", i.task_title));
    if !i.objective.trim().is_empty() {
        s.push_str(&format!("Goal: {}\n", i.objective.trim()));
    }
    if !i.constraints.is_empty() {
        s.push_str("Constraints:\n");
        for c in &i.constraints {
            s.push_str(&format!("  - {c}\n"));
        }
    }
    if !i.criteria.is_empty() {
        s.push_str("Requirements (status from Vibeke's evidence, not from agents):\n");
        for (id, text, req, st) in &i.criteria {
            s.push_str(&format!(
                "  - [{}] {text} ({}; {id})\n",
                st,
                if *req { "required" } else { "optional" }
            ));
        }
    }
    s.push_str(&format!("Stopping point: {}\n\n", i.stop_at));
    s.push_str(&format!(
        "What to review ({} subject {}):\n  git diff {} {}\n",
        i.subject_kind,
        &i.subject_id[..i.subject_id.len().min(12)],
        i.base_sha,
        i.content_sha
    ));
    if i.subject_kind == "dirty_snapshot" {
        s.push_str(
            "  (an immutable snapshot of uncommitted work; review that commit, not the live files)\n",
        );
    }
    if !i.files.is_empty() {
        s.push_str("Files:\n");
        for (p, a, r) in i.files.iter().take(60) {
            match (a, r) {
                (Some(a), Some(r)) => s.push_str(&format!("  {p} (+{a} -{r})\n")),
                _ => s.push_str(&format!("  {p} (binary)\n")),
            }
        }
        if i.files.len() > 60 {
            s.push_str(&format!("  … and {} more\n", i.files.len() - 60));
        }
    }
    if !i.checks.is_empty() {
        s.push_str("Recorded checks on this subject:\n");
        for c in &i.checks {
            s.push_str(&format!("  {c}\n"));
        }
    }
    s.push_str(
        "\nReport each finding on its own line as\n  FINDING [blocking|concern|nit] <file:line or area>: <what and why>\n\
         or a single line `NO FINDINGS` if you found nothing. Be specific; say when you are unsure.\n",
    );
    let (t, _) = truncate_utf8(&s, MAX_PROMPT_BYTES);
    t.to_string()
}

pub fn prompt_digest(prompt: &str) -> String {
    let mut h = FieldHasher::new("vk-review/reviewer-prompt/v1");
    h.str(prompt);
    h.finish()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocking,
    Concern,
    Nit,
    /// The reviewer said `NO FINDINGS`.
    NoFindings,
    /// Unstructured prose: treated as potentially blocking until classified.
    Unstructured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    #[default]
    Unassessed,
    Blocking,
    NotBlocking,
    Dismissed,
}

impl Classification {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "blocking" => Some(Classification::Blocking),
            "not_blocking" | "not-blocking" | "concern" => Some(Classification::NotBlocking),
            "dismissed" | "dismiss" => Some(Classification::Dismissed),
            "unassessed" => Some(Classification::Unassessed),
            _ => None,
        }
    }
}

/// An attributed review note (15 §6.1 "attributed findings").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewNote {
    pub id: String,
    pub task: String,
    /// The subject the reviewer was asked about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    /// Who said it: the reviewer run (agent) — never evidence.
    pub author: Actor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    pub severity: Severity,
    pub text: String,
    /// Always `agent_claim` for reviewer notes.
    pub category: String,
    #[serde(default)]
    pub classification: Classification,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classified_by: Option<Actor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification_reason: Option<String>,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classified_at_ms: Option<i64>,
}

impl ReviewNote {
    /// Needs a user decision before the stronger readiness label (§7).
    pub fn is_open_concern(&self) -> bool {
        match self.classification {
            Classification::Blocking => true,
            Classification::Unassessed => matches!(
                self.severity,
                Severity::Blocking | Severity::Concern | Severity::Unstructured
            ),
            Classification::NotBlocking | Classification::Dismissed => false,
        }
    }

    pub fn label(&self) -> &'static str {
        "Reviewer finding · agent opinion, not evidence"
    }
}

/// Split a reviewer's message into findings (`FINDING [sev] …` lines). A message without
/// structured lines becomes one `unstructured` finding; `NO FINDINGS` one `no_findings` note.
pub fn parse_findings(message: &str) -> Vec<(Severity, String)> {
    let mut out = Vec::new();
    for line in message.lines() {
        let l = line.trim().trim_start_matches(['-', '*', ' ']);
        let Some(rest) = l
            .strip_prefix("FINDING")
            .or_else(|| l.strip_prefix("Finding"))
        else {
            continue;
        };
        let rest = rest.trim_start();
        let (sev, text) = match rest.strip_prefix('[').and_then(|r| r.split_once(']')) {
            Some((sev, text)) => (
                match sev.trim().to_ascii_lowercase().as_str() {
                    "blocking" | "blocker" => Severity::Blocking,
                    "nit" | "minor" => Severity::Nit,
                    _ => Severity::Concern,
                },
                text.trim(),
            ),
            None => (Severity::Concern, rest),
        };
        let text = text.trim_start_matches(':').trim();
        if !text.is_empty() {
            out.push((sev, truncate_utf8(text, 2000).0.to_string()));
        }
    }
    if out.is_empty() {
        let m = message.trim();
        if m.is_empty() {
            return out;
        }
        if m.lines()
            .any(|l| l.trim().eq_ignore_ascii_case("no findings"))
        {
            out.push((Severity::NoFindings, "No findings".into()));
        } else {
            out.push((Severity::Unstructured, truncate_utf8(m, 4000).0.to_string()));
        }
    }
    out
}

/// Notes from one settled reviewer turn.
pub fn notes_from_turn(
    task: &str,
    subject_id: Option<&str>,
    run: &str,
    binding: &str,
    turn: u32,
    message: &str,
    now_ms: i64,
) -> Vec<ReviewNote> {
    parse_findings(message)
        .into_iter()
        .map(|(severity, text)| ReviewNote {
            id: new_id(),
            task: task.into(),
            subject_id: subject_id.map(str::to_string),
            author: Actor::agent(run),
            run: Some(run.into()),
            turn: Some(turn),
            binding: Some(binding.into()),
            severity,
            text,
            category: "agent_claim".into(),
            classification: Classification::Unassessed,
            classified_by: None,
            classification_reason: None,
            created_at_ms: now_ms,
            classified_at_ms: None,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClassifyError {
    #[error("only a user can classify review notes")]
    NotUser,
    #[error("a reason is required to dismiss a finding")]
    ReasonRequired,
}

/// A user's classification, with attribution (§7 "dismissed with attribution").
pub fn classify(
    note: &ReviewNote,
    to: Classification,
    actor: Actor,
    reason: Option<&str>,
    now_ms: i64,
) -> Result<ReviewNote, ClassifyError> {
    if actor.kind != ActorKind::User {
        return Err(ClassifyError::NotUser);
    }
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    if to == Classification::Dismissed && reason.is_none() {
        return Err(ClassifyError::ReasonRequired);
    }
    let mut n = note.clone();
    n.classification = to;
    n.classified_by = Some(actor);
    n.classification_reason = reason.map(str::to_string);
    n.classified_at_ms = Some(now_ms);
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_is_deterministic_and_review_only() {
        let i = PromptInput {
            task_title: "Fix login redirect".into(),
            objective: "Return to the original page".into(),
            criteria: vec![("c1".into(), "Preserve SSO".into(), true, "missing".into())],
            stop_at: "draft_pr".into(),
            subject_id: "abcdef0123456789".into(),
            subject_kind: "dirty_snapshot".into(),
            base_sha: "b".repeat(40),
            content_sha: "c".repeat(40),
            files: vec![
                ("a.rs".into(), Some(3), Some(1)),
                ("x.bin".into(), None, None),
            ],
            checks: vec!["unit: passed".into()],
            ..Default::default()
        };
        let p = build_prompt(&i);
        assert_eq!(p, build_prompt(&i));
        assert!(p.contains("Review only"));
        assert!(p.contains(&format!("git diff {} {}", "b".repeat(40), "c".repeat(40))));
        assert!(p.contains("immutable snapshot"));
        assert!(p.contains("[missing] Preserve SSO (required; c1)"));
        assert!(p.contains("x.bin (binary)"));
        assert_eq!(prompt_digest(&p), prompt_digest(&p));
        assert_ne!(prompt_digest(&p), prompt_digest(&format!("{p} ")));
    }

    #[test]
    fn findings_are_parsed_and_unstructured_prose_is_conservative() {
        let f = parse_findings(
            "Looked at it.\nFINDING [blocking] src/a.rs:10: token not validated\n- FINDING [nit] style\nFINDING missing test",
        );
        assert_eq!(
            f.iter().map(|x| x.0).collect::<Vec<_>>(),
            vec![Severity::Blocking, Severity::Nit, Severity::Concern]
        );
        assert_eq!(f[0].1, "src/a.rs:10: token not validated");
        assert_eq!(parse_findings("NO FINDINGS")[0].0, Severity::NoFindings);
        assert_eq!(
            parse_findings("Looks fine to me")[0].0,
            Severity::Unstructured
        );
        assert!(parse_findings("  ").is_empty());

        let notes = notes_from_turn("t", Some("s"), "run1", "b1", 1, "FINDING [nit] x\nNO", 5);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].author.kind, ActorKind::Agent);
        assert_eq!(notes[0].category, "agent_claim");
        assert!(!notes[0].is_open_concern(), "a nit does not block");
        let blocking = &notes_from_turn("t", None, "r", "b", 1, "FINDING [blocking] y", 5)[0];
        assert!(blocking.is_open_concern());
        // Only users classify; dismissing needs a reason and is attributed.
        assert_eq!(
            classify(
                blocking,
                Classification::Dismissed,
                Actor::agent("r"),
                Some("x"),
                6
            ),
            Err(ClassifyError::NotUser)
        );
        assert_eq!(
            classify(
                blocking,
                Classification::Dismissed,
                Actor::user("u"),
                None,
                6
            ),
            Err(ClassifyError::ReasonRequired)
        );
        let d = classify(
            blocking,
            Classification::Dismissed,
            Actor::user("u"),
            Some("false positive"),
            6,
        )
        .unwrap();
        assert!(!d.is_open_concern());
        assert_eq!(d.classified_by.unwrap().id, "u");
        let b = classify(
            blocking,
            Classification::Blocking,
            Actor::user("u"),
            None,
            6,
        )
        .unwrap();
        assert!(b.is_open_concern());
    }
}
