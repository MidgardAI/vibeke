//! Review-effort estimates (15 §8.2, T4).
//!
//! Effort is user-set (`task.set effort`). Two optional estimates help: a model estimate
//! through 14's `effort_estimate` operation (a draft the user applies explicitly) and this
//! deterministic heuristic over the review package's diff size, files touched and failing
//! checks. Both are labelled estimates with their source; neither is applied automatically or
//! used as if the user had set it.

use serde::{Deserialize, Serialize};

use crate::attention::Effort;

/// Inputs the heuristic looks at (all observable from the review package).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EffortInputs {
    pub files: usize,
    /// Added + removed lines (binary files count as files only).
    pub lines: u64,
    pub binary_files: usize,
    /// Required checks failed / unknown on the reviewed subject.
    pub failing_checks: usize,
    /// Criteria that need human judgment.
    pub human_criteria: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortEstimate {
    pub effort: Effort,
    /// Always `heuristic` here; a model estimate carries `assistant` and its request id.
    pub source: String,
    pub label: String,
    pub reasons: Vec<String>,
}

pub const HEURISTIC_LABEL: &str = "Heuristic estimate (diff size, files, checks) — not set";

/// Deterministic coarse estimate. No diff ⇒ `unknown`.
pub fn heuristic(i: &EffortInputs) -> EffortEstimate {
    let mut reasons = vec![format!(
        "{} file(s), {} changed line(s){}",
        i.files,
        i.lines,
        if i.binary_files > 0 {
            format!(", {} binary", i.binary_files)
        } else {
            String::new()
        }
    )];
    let mut level: u8 = if i.files == 0 {
        reasons = vec!["no diff to review".into()];
        return EffortEstimate {
            effort: Effort::Unknown,
            source: "heuristic".into(),
            label: HEURISTIC_LABEL.into(),
            reasons,
        };
    } else if i.lines <= 40 && i.files <= 3 {
        0
    } else if i.lines <= 400 && i.files <= 15 {
        1
    } else {
        2
    };
    if i.failing_checks > 0 {
        reasons.push(format!("{} failing/unknown check(s)", i.failing_checks));
        level = (level + 1).min(2);
    }
    if i.human_criteria >= 3 && level == 0 {
        reasons.push(format!("{} criteria need your judgment", i.human_criteria));
        level = 1;
    }
    EffortEstimate {
        effort: match level {
            0 => Effort::Quick,
            1 => Effort::FewMinutes,
            _ => Effort::DeepReview,
        },
        source: "heuristic".into(),
        label: HEURISTIC_LABEL.into(),
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coarse_levels_and_bumps() {
        let q = heuristic(&EffortInputs {
            files: 1,
            lines: 10,
            ..Default::default()
        });
        assert_eq!(q.effort, Effort::Quick);
        assert_eq!(q.source, "heuristic");
        let m = heuristic(&EffortInputs {
            files: 6,
            lines: 120,
            ..Default::default()
        });
        assert_eq!(m.effort, Effort::FewMinutes);
        let d = heuristic(&EffortInputs {
            files: 40,
            lines: 3000,
            ..Default::default()
        });
        assert_eq!(d.effort, Effort::DeepReview);
        let bumped = heuristic(&EffortInputs {
            files: 1,
            lines: 10,
            failing_checks: 1,
            ..Default::default()
        });
        assert_eq!(bumped.effort, Effort::FewMinutes);
        assert!(bumped.reasons.iter().any(|r| r.contains("failing")));
        assert_eq!(heuristic(&EffortInputs::default()).effort, Effort::Unknown);
    }
}
