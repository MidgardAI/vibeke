//! Step outcomes and the single attribution rule (18 §8.4).

use serde::{Deserialize, Serialize};

/// Ordered from best to worst; a plugin's outcome is its worst step outcome.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Passed,
    NotExercised,
    DependencyFailure,
    SuspectedCompatFailure,
    CompatFailure,
    HarnessError,
}

/// What one side did with a step.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SideResult {
    Passed,
    Failed,
    /// The side did not run the step (for Herdr: not run yet in this mode).
    NotRun,
    /// The harness, runner or scenario broke while running this side.
    HarnessError,
}

/// Facts the rule needs for one step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepFacts {
    pub vibeke: SideResult,
    pub herdr: SideResult,
    /// Both sides received the same dependency bytes: guaranteed in verified mode, compared by
    /// download hashes in unpinned mode. `None` when the bytes were not compared: Herdr did not
    /// run, or the harness did not observe the unpinned downloads. Rule 3 then gives only a
    /// `suspected_compat_failure`.
    pub same_dependency_bytes: Option<bool>,
    /// A build or external dependency failed on the Vibeke side before the step could run.
    pub dependency_failed: bool,
    /// The result breaks the pinned schema (a `compat_failure` even without a Herdr run).
    pub schema_violation: bool,
    /// Why the Herdr side failed, if it did. Rule 4 uses it as the reason.
    pub herdr_reason: Option<String>,
}

/// The rule, in order:
/// 0. A harness error on either side is `harness_error`; a dependency failure is
///    `dependency_failure`; a schema violation is `compat_failure`.
/// 1. Different dependency bytes on the two sides: `dependency_failure`.
/// 2. Herdr did not run the step: a Vibeke failure is `suspected_compat_failure`.
/// 3. Herdr passes and Vibeke fails: `compat_failure`. If nobody compared the dependency bytes
///    (`same_dependency_bytes` is `None`), the failure is only `suspected_compat_failure`.
/// 4. Both fail: `not_exercised`, with the Herdr result as the reason.
/// Otherwise `passed`.
///
/// Cases the rule does not name: Vibeke passing is `passed` whatever Herdr did, and a Vibeke
/// side that did not run is `not_exercised`.
pub fn attribute(f: &StepFacts) -> (Outcome, Option<String>) {
    use SideResult as S;
    let why = |o: Outcome, r: &str| (o, Some(r.to_string()));
    // Rule 0.
    if f.vibeke == S::HarnessError {
        return why(Outcome::HarnessError, "harness error on the Vibeke side");
    }
    if f.herdr == S::HarnessError {
        return why(Outcome::HarnessError, "harness error on the Herdr side");
    }
    if f.dependency_failed {
        return why(
            Outcome::DependencyFailure,
            "a build or external dependency failed",
        );
    }
    if f.schema_violation {
        return why(
            Outcome::CompatFailure,
            "the result breaks the pinned schema",
        );
    }
    // Rule 1.
    if f.same_dependency_bytes == Some(false) {
        return why(
            Outcome::DependencyFailure,
            "the two sides received different dependency bytes",
        );
    }
    match (f.vibeke, f.herdr) {
        (S::NotRun, _) => why(Outcome::NotExercised, "Vibeke did not run the step"),
        (S::Passed, _) => (Outcome::Passed, None),
        // Rule 2.
        (S::Failed, S::NotRun) => why(
            Outcome::SuspectedCompatFailure,
            "Vibeke failed and Herdr has not run the step",
        ),
        // Rule 3, without proof that both sides had the same dependency bytes.
        (S::Failed, S::Passed) if f.same_dependency_bytes.is_none() => why(
            Outcome::SuspectedCompatFailure,
            "dependency bytes not compared (unpinned)",
        ),
        // Rule 3.
        (S::Failed, S::Passed) => why(
            Outcome::CompatFailure,
            "Herdr passes the step and Vibeke fails it",
        ),
        // Rule 4.
        (S::Failed, S::Failed) => (
            Outcome::NotExercised,
            Some(format!(
                "Herdr failed too: {}",
                f.herdr_reason.as_deref().unwrap_or("no reason recorded")
            )),
        ),
        (_, S::HarnessError) | (S::HarnessError, _) => unreachable!("handled by rule 0"),
    }
}

/// The final outcome of a step with a `known` result: a known step that now passes is an
/// unexpected pass and fails the run; a known step whose outcome differs from the recorded one
/// keeps its new outcome (the scenario must be updated).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconciled {
    pub outcome: Outcome,
    pub unexpected_pass: bool,
    /// The known outcome matched exactly; this step does not fail the run.
    pub matches_known: bool,
}

pub fn reconcile(actual: Outcome, known: Option<Outcome>) -> Reconciled {
    match known {
        Some(_) if actual == Outcome::Passed => Reconciled {
            outcome: Outcome::Passed,
            unexpected_pass: true,
            matches_known: false,
        },
        Some(k) => Reconciled {
            outcome: actual,
            unexpected_pass: false,
            matches_known: k == actual,
        },
        None => Reconciled {
            outcome: actual,
            unexpected_pass: false,
            matches_known: false,
        },
    }
}

/// The worst outcome, `Passed` for an empty list.
pub fn worst(outcomes: impl IntoIterator<Item = Outcome>) -> Outcome {
    outcomes.into_iter().max().unwrap_or(Outcome::Passed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use SideResult as S;

    fn facts(vibeke: SideResult, herdr: SideResult) -> StepFacts {
        StepFacts {
            vibeke,
            herdr,
            same_dependency_bytes: Some(true),
            dependency_failed: false,
            schema_violation: false,
            herdr_reason: None,
        }
    }

    fn outcome(f: &StepFacts) -> Outcome {
        attribute(f).0
    }

    #[test]
    fn rule_zero() {
        assert_eq!(
            outcome(&facts(S::HarnessError, S::Passed)),
            Outcome::HarnessError
        );
        assert_eq!(
            outcome(&facts(S::Passed, S::HarnessError)),
            Outcome::HarnessError
        );
        let mut f = facts(S::Failed, S::Passed);
        f.dependency_failed = true;
        assert_eq!(outcome(&f), Outcome::DependencyFailure);
        let mut f = facts(S::Passed, S::NotRun);
        f.same_dependency_bytes = None;
        f.schema_violation = true;
        assert_eq!(outcome(&f), Outcome::CompatFailure);
        // A harness error wins over everything else.
        let mut f = facts(S::HarnessError, S::Failed);
        f.dependency_failed = true;
        f.schema_violation = true;
        assert_eq!(outcome(&f), Outcome::HarnessError);
    }

    #[test]
    fn rule_one_different_bytes() {
        let mut f = facts(S::Failed, S::Passed);
        f.same_dependency_bytes = Some(false);
        assert_eq!(outcome(&f), Outcome::DependencyFailure);
        let mut f = facts(S::Passed, S::Passed);
        f.same_dependency_bytes = Some(false);
        assert_eq!(outcome(&f), Outcome::DependencyFailure);
    }

    #[test]
    fn rule_two_herdr_not_run() {
        let mut f = facts(S::Failed, S::NotRun);
        f.same_dependency_bytes = None;
        let (o, r) = attribute(&f);
        assert_eq!(o, Outcome::SuspectedCompatFailure);
        assert!(r.is_some());
        f.vibeke = S::Passed;
        assert_eq!(attribute(&f), (Outcome::Passed, None));
    }

    #[test]
    fn rule_three_herdr_passes() {
        assert_eq!(
            outcome(&facts(S::Failed, S::Passed)),
            Outcome::CompatFailure
        );
    }

    #[test]
    fn rule_three_needs_compared_bytes() {
        let mut f = facts(S::Failed, S::Passed);
        f.same_dependency_bytes = None;
        assert_eq!(
            attribute(&f),
            (
                Outcome::SuspectedCompatFailure,
                Some("dependency bytes not compared (unpinned)".to_string())
            )
        );
        // Verified mode sets `Some(true)` and confirms the failure.
        f.same_dependency_bytes = Some(true);
        assert_eq!(outcome(&f), Outcome::CompatFailure);
        // Uncompared bytes do not change the other cases.
        f.same_dependency_bytes = None;
        f.vibeke = S::Passed;
        assert_eq!(attribute(&f), (Outcome::Passed, None));
        let mut f = facts(S::Failed, S::Failed);
        f.same_dependency_bytes = None;
        assert_eq!(outcome(&f), Outcome::NotExercised);
        // A schema violation stays a confirmed failure without compared bytes.
        let mut f = facts(S::Failed, S::Passed);
        f.same_dependency_bytes = None;
        f.schema_violation = true;
        assert_eq!(outcome(&f), Outcome::CompatFailure);
    }

    #[test]
    fn rule_four_both_fail() {
        let mut f = facts(S::Failed, S::Failed);
        f.herdr_reason = Some("popup.open: not_found".into());
        let (o, r) = attribute(&f);
        assert_eq!(o, Outcome::NotExercised);
        assert_eq!(
            r.as_deref(),
            Some("Herdr failed too: popup.open: not_found")
        );
        f.herdr_reason = None;
        assert!(attribute(&f).1.unwrap().starts_with("Herdr failed too: "));
    }

    #[test]
    fn otherwise_passed_or_not_run() {
        assert_eq!(
            attribute(&facts(S::Passed, S::Passed)),
            (Outcome::Passed, None)
        );
        assert_eq!(outcome(&facts(S::Passed, S::Failed)), Outcome::Passed);
        let (o, r) = attribute(&facts(S::NotRun, S::Passed));
        assert_eq!(o, Outcome::NotExercised);
        assert!(r.is_some(), "not_exercised always has a reason");
    }

    #[test]
    fn reconcile_known_results() {
        let r = reconcile(Outcome::Passed, Some(Outcome::CompatFailure));
        assert_eq!(
            r,
            Reconciled {
                outcome: Outcome::Passed,
                unexpected_pass: true,
                matches_known: false
            }
        );
        let r = reconcile(Outcome::CompatFailure, Some(Outcome::CompatFailure));
        assert!(r.matches_known && !r.unexpected_pass);
        assert_eq!(r.outcome, Outcome::CompatFailure);
        let r = reconcile(Outcome::HarnessError, Some(Outcome::CompatFailure));
        assert!(!r.matches_known && !r.unexpected_pass);
        assert_eq!(r.outcome, Outcome::HarnessError);
        let r = reconcile(Outcome::Passed, None);
        assert!(!r.matches_known && !r.unexpected_pass);
        let r = reconcile(Outcome::CompatFailure, None);
        assert_eq!(r.outcome, Outcome::CompatFailure);
        assert!(!r.matches_known);
    }

    #[test]
    fn worst_ordering() {
        assert_eq!(worst(Vec::new()), Outcome::Passed);
        assert_eq!(
            worst([Outcome::Passed, Outcome::NotExercised]),
            Outcome::NotExercised
        );
        assert_eq!(
            worst([
                Outcome::CompatFailure,
                Outcome::DependencyFailure,
                Outcome::SuspectedCompatFailure
            ]),
            Outcome::CompatFailure
        );
        assert_eq!(
            worst([Outcome::HarnessError, Outcome::CompatFailure]),
            Outcome::HarnessError
        );
        assert!(Outcome::NotExercised < Outcome::DependencyFailure);
        assert!(Outcome::DependencyFailure < Outcome::SuspectedCompatFailure);
    }

    #[test]
    fn outcome_spelling() {
        assert_eq!(
            serde_json::to_string(&Outcome::SuspectedCompatFailure).unwrap(),
            "\"suspected_compat_failure\""
        );
    }
}
