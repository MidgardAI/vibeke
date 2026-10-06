//! Evaluation harness for briefings (14 §11, A1 acceptance 7).
//!
//! Measures, per fixture case, a generated briefing against the **structured-list baseline**
//! (the plain attention list a user has without an LLM):
//!
//! - *factual support* (mechanical proxy): the share of items whose cited sources exist and
//!   share a significant term with the item's text;
//! - *important blocked items omitted*: blocked targets (open approvals) the briefing never
//!   links;
//! - *incorrect urgency*: items linked to a target whose expected urgency differs;
//! - *time to find*: the position of the interaction the user needs in the item list.
//!
//! The harness is deterministic and runs against canned model outputs or the fake provider.
//! It cannot judge semantic truth: the factual-support number is a screening proxy, and the
//! real evaluation needs a live model and a human judge (audit section 4, item 13). Record
//! model and prompt versions with every run before enabling background features.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct Target {
    pub id: String,
    pub label: String,
    pub urgency: String,
    /// An open approval or other item blocking an agent.
    pub blocked: bool,
}

#[derive(Debug, Clone)]
pub struct Case {
    pub id: String,
    /// Source id -> text the briefing may cite.
    pub sources: BTreeMap<String, String>,
    pub targets: Vec<Target>,
    /// The target the user needs to find first (time-to-find).
    pub needed: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Score {
    pub items: usize,
    pub supported: usize,
    pub blocked_total: usize,
    pub blocked_omitted: usize,
    pub urgency_checked: usize,
    pub urgency_wrong: usize,
    /// 1-based position of the needed target; `None` when absent.
    pub time_to_find: Option<usize>,
}

fn significant(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(str::to_lowercase)
        .collect()
}

/// Score a Briefing-shaped output (`{items:[{text, urgency, targets, source_refs}]}`).
pub fn score(case: &Case, output: &Value) -> Score {
    let items = output["items"].as_array().cloned().unwrap_or_default();
    let mut supported = 0;
    let mut linked: Vec<String> = vec![];
    let mut urgency_checked = 0;
    let mut urgency_wrong = 0;
    let mut time_to_find = None;
    for (n, it) in items.iter().enumerate() {
        let words = significant(it["text"].as_str().unwrap_or(""));
        let refs: Vec<&str> = it["source_refs"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let ok = !refs.is_empty()
            && refs.iter().all(|r| {
                case.sources.get(*r).is_some_and(|text| {
                    let hay = significant(text);
                    words.iter().any(|w| hay.contains(w))
                })
            });
        if ok {
            supported += 1;
        }
        for t in it["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            linked.push(t.to_string());
            if let Some(target) = case.targets.iter().find(|x| x.id == t) {
                urgency_checked += 1;
                if it["urgency"].as_str().unwrap_or("fyi") != target.urgency {
                    urgency_wrong += 1;
                }
            }
            if case.needed.as_deref() == Some(t) && time_to_find.is_none() {
                time_to_find = Some(n + 1);
            }
        }
    }
    let blocked: Vec<&Target> = case.targets.iter().filter(|t| t.blocked).collect();
    Score {
        items: items.len(),
        supported,
        blocked_total: blocked.len(),
        blocked_omitted: blocked.iter().filter(|t| !linked.contains(&t.id)).count(),
        urgency_checked,
        urgency_wrong,
        time_to_find,
    }
}

/// The structured-list baseline: every target as one item, blocked first, each with its own
/// recorded urgency. It cites no sources (it is Vibeke's own state), so it is scored for
/// omissions, urgency and time-to-find only.
pub fn baseline(case: &Case) -> Value {
    let mut ts = case.targets.clone();
    ts.sort_by_key(|t| !t.blocked);
    json!({"items": ts.iter().map(|t| json!({
        "text": t.label, "urgency": t.urgency, "targets": [t.id], "source_refs": [],
    })).collect::<Vec<_>>()})
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub cases: usize,
    pub model: Score,
    pub baseline: Score,
    /// Fraction of model items with mechanical factual support.
    pub factual_support: f64,
    pub notes: Vec<String>,
}

fn sum(a: &mut Score, b: &Score) {
    a.items += b.items;
    a.supported += b.supported;
    a.blocked_total += b.blocked_total;
    a.blocked_omitted += b.blocked_omitted;
    a.urgency_checked += b.urgency_checked;
    a.urgency_wrong += b.urgency_wrong;
    a.time_to_find = match (a.time_to_find, b.time_to_find) {
        (Some(x), Some(y)) => Some(x + y),
        (x, None) | (None, x) => x,
    };
}

fn zero() -> Score {
    Score {
        items: 0,
        supported: 0,
        blocked_total: 0,
        blocked_omitted: 0,
        urgency_checked: 0,
        urgency_wrong: 0,
        time_to_find: None,
    }
}

/// Score `outputs` (one per case, in order) against the baseline over the same cases.
pub fn evaluate(cases: &[Case], outputs: &[Value], model_prompt: &str) -> Report {
    let mut m = zero();
    let mut b = zero();
    for (c, o) in cases.iter().zip(outputs) {
        sum(&mut m, &score(c, o));
        sum(&mut b, &score(c, &baseline(c)));
    }
    let mut notes = vec![
        format!("model/prompt: {model_prompt}"),
        "factual support is a mechanical proxy (shared terms); semantic truth needs a human judge"
            .into(),
        "time_to_find is summed over cases that contained the needed item".into(),
    ];
    if m.blocked_omitted > b.blocked_omitted {
        notes.push("the briefing omitted blocked items the plain list shows".into());
    }
    Report {
        cases: cases.len().min(outputs.len()),
        factual_support: if m.items == 0 {
            1.0
        } else {
            m.supported as f64 / m.items as f64
        },
        model: m,
        baseline: b,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case() -> Case {
        Case {
            id: "c1".into(),
            sources: BTreeMap::from([
                (
                    "s1".to_string(),
                    "Agent wants to run the database migration on production".to_string(),
                ),
                (
                    "s2".to_string(),
                    "Tests are running for the login redirect".to_string(),
                ),
            ]),
            targets: vec![
                Target {
                    id: "i1".into(),
                    label: "migration approval".into(),
                    urgency: "now".into(),
                    blocked: true,
                },
                Target {
                    id: "r2".into(),
                    label: "login tests".into(),
                    urgency: "fyi".into(),
                    blocked: false,
                },
            ],
            needed: Some("i1".into()),
        }
    }

    #[test]
    fn a_good_briefing_scores_clean() {
        let out = json!({"items": [
            {"text": "Approve or deny the production database migration", "urgency": "now", "targets": ["i1"], "source_refs": ["s1"]},
            {"text": "Login redirect tests are running", "urgency": "fyi", "targets": ["r2"], "source_refs": ["s2"]},
        ]});
        let s = score(&case(), &out);
        assert_eq!((s.items, s.supported), (2, 2));
        assert_eq!((s.blocked_total, s.blocked_omitted), (1, 0));
        assert_eq!((s.urgency_checked, s.urgency_wrong), (2, 0));
        assert_eq!(s.time_to_find, Some(1));
    }

    #[test]
    fn omissions_wrong_urgency_and_unsupported_claims_are_counted() {
        let out = json!({"items": [
            {"text": "Everything is calm", "urgency": "fyi", "targets": ["r2"], "source_refs": ["s2"]},
            {"text": "Quarterly planning", "urgency": "soon", "targets": [], "source_refs": ["s9"]},
        ]});
        let s = score(&case(), &out);
        assert_eq!(s.supported, 0, "no shared terms / unknown source");
        assert_eq!(s.blocked_omitted, 1);
        assert_eq!(s.time_to_find, None);
        assert_eq!(s.urgency_wrong, 0);
        let wrong = json!({"items": [
            {"text": "migration is urgent", "urgency": "fyi", "targets": ["i1"], "source_refs": ["s1"]},
        ]});
        assert_eq!(score(&case(), &wrong).urgency_wrong, 1);
    }

    #[test]
    fn the_baseline_never_omits_and_lists_blocked_first() {
        let c = case();
        let b = baseline(&c);
        assert_eq!(b["items"][0]["targets"][0], "i1");
        let s = score(&c, &b);
        assert_eq!(s.blocked_omitted, 0);
        assert_eq!(s.urgency_wrong, 0);
        assert_eq!(s.time_to_find, Some(1));
    }

    #[test]
    fn the_report_compares_against_the_baseline_and_records_provenance() {
        let c = case();
        let bad = json!({"items": [{"text": "calm", "urgency": "fyi", "targets": ["r2"], "source_refs": []}]});
        let r = evaluate(&[c], &[bad], "claude-haiku-4-5 / prompt v1");
        assert_eq!(r.cases, 1);
        assert_eq!(r.model.blocked_omitted, 1);
        assert_eq!(r.baseline.blocked_omitted, 0);
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("claude-haiku-4-5 / prompt v1"))
        );
        assert!(r.notes.iter().any(|n| n.contains("omitted blocked items")));
        assert!(r.notes.iter().any(|n| n.contains("human judge")));
        assert_eq!(r.factual_support, 0.0);
    }
}
