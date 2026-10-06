//! Vibeke run state → Herdr agent status (07 §8.3: "project execution/attention/read state into
//! the baseline's agent status schema").
//!
//! Herdr's self-report vocabulary is `idle | working | blocked`; its UI adds `done` (idle after
//! work, not yet seen) and `unknown`. Structured adapters stay Vibeke's source of truth; native
//! states such as `rate_limited` or `needs_answer` are folded into these five and never leak.
//! The exact baseline enumeration is *unverified* until the schema snapshot is captured.

/// Herdr agent statuses, most urgent first.
pub const STATUSES: &[&str] = &["blocked", "done", "working", "idle", "unknown"];

/// The Herdr status of one run.
///
/// * `execution` — Vibeke execution state (`starting working idle error rate_limited exited
///   unknown`);
/// * `open_interaction` — the run has an open approval/question (Vibeke `needs_*`);
/// * `unseen_done` — the turn finished and no client has looked at the pane yet.
pub fn agent_status(execution: &str, open_interaction: bool, unseen_done: bool) -> &'static str {
    if open_interaction {
        return "blocked";
    }
    match execution {
        "working" | "starting" | "rate_limited" => "working",
        "idle" if unseen_done => "done",
        "idle" => "idle",
        _ => "unknown",
    }
}

/// The most urgent status of a set (workspace `agent_status`); `None` when there is no agent.
pub fn most_urgent<'a>(statuses: impl IntoIterator<Item = &'a str>) -> Option<&'static str> {
    statuses
        .into_iter()
        .filter_map(|s| STATUSES.iter().position(|x| *x == s))
        .min()
        .map(|i| STATUSES[i])
}

/// Herdr agent names → Vibeke harness ids (`claude-code` → `claude`, …) and back.
pub fn herdr_agent_name(harness: &str) -> &str {
    match harness {
        "omp" => "oh-my-pi",
        h => h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping() {
        assert_eq!(agent_status("working", false, false), "working");
        assert_eq!(agent_status("idle", true, false), "blocked");
        assert_eq!(agent_status("idle", false, true), "done");
        assert_eq!(agent_status("idle", false, false), "idle");
        assert_eq!(agent_status("rate_limited", false, false), "working");
        assert_eq!(agent_status("exited", false, false), "unknown");
        assert_eq!(most_urgent(["idle", "done", "working"]), Some("done"));
        assert_eq!(most_urgent(["idle", "blocked"]), Some("blocked"));
        assert_eq!(most_urgent([]), None);
        assert_eq!(herdr_agent_name("omp"), "oh-my-pi");
    }
}
