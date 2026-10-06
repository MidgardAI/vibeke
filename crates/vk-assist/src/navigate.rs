//! Semantic navigation candidates (14 §2, A2).
//!
//! The coordinator offers the model only **authorized candidates**: objects of consented
//! workspaces with their metadata. A deterministic lexical pre-rank keeps the list bounded (the
//! most plausible `limit` candidates are sent, the rest are reported as omitted), so a large
//! session never sends everything. The model re-ranks that list; its answer is validated
//! against the candidate IDs, so it can only point at objects that were offered.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// The object's own ID (the only thing a result may cite as a target).
    pub id: String,
    /// `pane | run | task | interaction | workspace`.
    pub kind: String,
    pub label: String,
    /// Metadata text shown to the model (already bounded by the caller).
    pub text: String,
    /// Recency tiebreak: larger is more recent.
    pub recency: i64,
}

const STOP: &[&str] = &[
    "the", "and", "for", "with", "our", "that", "this", "find", "show", "where", "what", "which",
    "agent", "about", "from", "into", "are", "was", "were", "has", "have", "can", "you", "all",
];

/// Lowercased alphanumeric tokens of at least three characters, without stop words.
pub fn tokens(s: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if cur.chars().count() >= 3 && !STOP.contains(&cur.as_str()) && !out.contains(cur) {
            out.push(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };
    for c in s.chars() {
        if c.is_alphanumeric() {
            cur.extend(c.to_lowercase());
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// Overlap score: label hits count double, with a prefix match for light stemming
/// (`refund` matches `refunds`).
pub fn score(query: &[String], c: &Candidate) -> u32 {
    let label = tokens(&c.label);
    let text = tokens(&c.text);
    let hit = |hay: &[String], q: &str| {
        hay.iter().any(|h| {
            h == q
                || (q.len() >= 4 && (h.starts_with(q) || q.starts_with(h.as_str()) && h.len() >= 4))
        })
    };
    query
        .iter()
        .map(|q| {
            let mut s = 0;
            if hit(&label, q) {
                s += 2;
            }
            if hit(&text, q) {
                s += 1;
            }
            s
        })
        .sum()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub candidates: Vec<Candidate>,
    /// Candidates left out by the bound.
    pub omitted: usize,
    /// No candidate shared a term with the query: the order is recency only.
    pub unranked: bool,
}

/// The `limit` most plausible candidates, best first (ties by recency, then input order).
pub fn rank(query: &str, mut cands: Vec<Candidate>, limit: usize) -> Ranked {
    let q = tokens(query);
    let mut scored: Vec<(u32, usize, Candidate)> = cands
        .drain(..)
        .enumerate()
        .map(|(i, c)| (score(&q, &c), i, c))
        .collect();
    let unranked = scored.iter().all(|(s, _, _)| *s == 0);
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.2.recency.cmp(&a.2.recency))
            .then(a.1.cmp(&b.1))
    });
    let total = scored.len();
    let candidates: Vec<Candidate> = scored.into_iter().take(limit).map(|x| x.2).collect();
    Ranked {
        omitted: total - candidates.len(),
        candidates,
        unranked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, label: &str, text: &str, recency: i64) -> Candidate {
        Candidate {
            id: id.into(),
            kind: "run".into(),
            label: label.into(),
            text: text.into(),
            recency,
        }
    }

    #[test]
    fn tokens_drop_stop_words_and_short_words() {
        assert_eq!(
            tokens("Find the agent fixing login, in OUR repo"),
            vec!["fixing", "login", "repo"]
        );
        assert!(tokens("a an of").is_empty());
    }

    #[test]
    fn label_hits_outrank_text_hits_and_stems_match() {
        let cands = vec![
            c("r1", "docs agent", "writing readme", 5),
            c("r2", "refunds agent", "implementing partial refund flow", 1),
            c("r3", "ci", "login redirect fix mentions refunds once", 9),
        ];
        let r = rank("our discussion about refunds", cands, 10);
        assert!(!r.unranked);
        let ids: Vec<&str> = r.candidates.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["r2", "r3", "r1"]);
    }

    #[test]
    fn the_list_is_bounded_and_reports_what_was_left_out() {
        let cands: Vec<Candidate> = (0..10)
            .map(|i| c(&format!("r{i}"), "worker", "text", i))
            .collect();
        let r = rank("login", cands, 4);
        assert_eq!(r.candidates.len(), 4);
        assert_eq!(r.omitted, 6);
        assert!(r.unranked, "no term overlap: recency order only");
        assert_eq!(r.candidates[0].id, "r9");
    }

    #[test]
    fn empty_input_is_fine() {
        let r = rank("anything", vec![], 5);
        assert!(r.candidates.is_empty() && r.omitted == 0 && r.unranked);
    }
}
