//! Possible-stall signals (14 §2, A3): deterministic repetition detection that decides whether
//! a background stall check is even worth a provider call.
//!
//! The detector looks at the tail of a run's recorded commands: the same normalized command
//! failing (non-zero exit) `threshold` or more times in a row is a signal. It never reads
//! output and never decides that an agent is stuck: the model is asked only when a signal
//! exists, and its notice is a passive, opt-in, labelled interpretation.

use serde::{Deserialize, Serialize};

/// One recorded command outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct Obs {
    pub command: String,
    pub exit_code: Option<i32>,
    pub ended_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signal {
    /// The repeated command, normalized and bounded (redaction happens when it becomes a
    /// source).
    pub command: String,
    pub repeats: u32,
    pub exit_codes: Vec<i32>,
    pub first_ms: Option<i64>,
    pub last_ms: Option<i64>,
}

/// Collapse whitespace and bound the length so trivially different spellings compare equal.
pub fn normalize(cmd: &str) -> String {
    let n: Vec<&str> = cmd.split_whitespace().collect();
    let n = n.join(" ");
    crate::clip(&n, 200).0
}

/// The trailing run of identical failing commands, if it has at least `threshold` entries.
/// A passing or unknown-outcome command, or a different command, ends the run.
pub fn detect(obs: &[Obs], threshold: u32) -> Option<Signal> {
    let threshold = threshold.max(2);
    let mut run: Vec<&Obs> = vec![];
    let mut cmd: Option<String> = None;
    for o in obs.iter().rev() {
        let failed = matches!(o.exit_code, Some(c) if c != 0);
        let n = normalize(&o.command);
        if !failed || n.is_empty() {
            break;
        }
        match &cmd {
            None => cmd = Some(n),
            Some(c) if *c == n => {}
            Some(_) => break,
        }
        run.push(o);
    }
    if (run.len() as u32) < threshold {
        return None;
    }
    run.reverse();
    Some(Signal {
        command: cmd.unwrap_or_default(),
        repeats: run.len() as u32,
        exit_codes: run.iter().filter_map(|o| o.exit_code).collect(),
        first_ms: run.first().and_then(|o| o.ended_at_ms),
        last_ms: run.last().and_then(|o| o.ended_at_ms),
    })
}

impl Signal {
    /// Stable identity of this stall for de-duplicating notices: the command and how many
    /// failures. A longer loop is a new signal; the same loop is not re-announced.
    pub fn digest(&self) -> String {
        blake3::hash(format!("{}\u{0}{}", self.command, self.repeats).as_bytes()).to_hex()[..16]
            .to_string()
    }

    /// Plain-text source for the model: facts only.
    pub fn render(&self) -> String {
        format!(
            "The command `{}` failed {} times in a row (exit codes: {}).",
            self.command,
            self.repeats,
            self.exit_codes
                .iter()
                .map(i32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(c: &str, code: Option<i32>, t: i64) -> Obs {
        Obs {
            command: c.into(),
            exit_code: code,
            ended_at_ms: Some(t),
        }
    }

    #[test]
    fn four_identical_failures_are_a_signal() {
        let obs = vec![
            o("ls", Some(0), 1),
            o("npm  install", Some(1), 2),
            o("npm install", Some(1), 3),
            o("npm install ", Some(1), 4),
            o("npm install", Some(1), 5),
        ];
        let s = detect(&obs, 3).unwrap();
        assert_eq!(s.command, "npm install");
        assert_eq!(s.repeats, 4);
        assert_eq!(s.exit_codes, vec![1, 1, 1, 1]);
        assert_eq!((s.first_ms, s.last_ms), (Some(2), Some(5)));
        assert!(s.render().contains("failed 4 times"));
    }

    #[test]
    fn a_pass_a_different_command_or_an_unknown_outcome_ends_the_run() {
        let base = |last: Obs| {
            vec![
                o("make", Some(2), 1),
                o("make", Some(2), 2),
                o("make", Some(2), 3),
                last,
            ]
        };
        assert!(
            detect(&base(o("make", Some(0), 4)), 3).is_none(),
            "passed after"
        );
        assert!(
            detect(&base(o("ls", Some(1), 4)), 3).is_none(),
            "different command last"
        );
        assert!(
            detect(&base(o("make", None, 4)), 3).is_none(),
            "unknown outcome"
        );
        assert!(detect(&base(o("make", Some(2), 4)), 3).is_some());
    }

    #[test]
    fn below_threshold_and_degenerate_inputs_are_quiet() {
        assert!(detect(&[], 3).is_none());
        let two = vec![o("x", Some(1), 1), o("x", Some(1), 2)];
        assert!(detect(&two, 3).is_none());
        assert!(detect(&two, 2).is_some());
        // The threshold never drops below two: one failure is not a loop.
        assert!(detect(&two[..1], 0).is_none());
        assert!(detect(&[o("", Some(1), 1), o("", Some(1), 2)], 2).is_none());
    }

    #[test]
    fn digest_identifies_the_loop_and_its_length() {
        let a = detect(
            &[o("x", Some(1), 1), o("x", Some(1), 2), o("x", Some(1), 3)],
            3,
        )
        .unwrap();
        let b = detect(
            &[o("x", Some(1), 5), o("x", Some(1), 6), o("x", Some(1), 7)],
            3,
        )
        .unwrap();
        assert_eq!(a.digest(), b.digest(), "the same loop at another time");
        let longer = detect(
            &[
                o("x", Some(1), 1),
                o("x", Some(1), 2),
                o("x", Some(1), 3),
                o("x", Some(1), 4),
            ],
            3,
        )
        .unwrap();
        assert_ne!(a.digest(), longer.digest());
    }
}
