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

/// Herdr's `is_official_agent_source`: a release from one of Herdr's own integration hooks
/// (`herdr:<agent>`) is ignored, because the process detector owns those agents.
pub fn official_agent_source(source: &str, agent: &str) -> bool {
    let Some(name) = source.strip_prefix("herdr:") else {
        return false;
    };
    match name {
        "antigravity_cli" => agent == "agy",
        "claude" | "codex" | "copilot" | "devin" | "droid" | "kimi" | "omp" | "mastracode"
        | "pi" | "hermes" | "opencode" | "qodercli" | "qwen" | "kilo" | "cursor" | "grok"
        | "letta" => agent == name,
        _ => false,
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

/// The Vibeke harness id for a Herdr agent name (`agent.start`).
pub fn vibeke_harness(agent: &str) -> &str {
    match agent {
        "oh-my-pi" => "omp",
        "claude-code" => "claude",
        a => a,
    }
}

/// Herdr's agent lookup (`src/detect/mod.rs`, `parse_agent_label` and `lookup_agent`): a
/// process or label name, lower-cased, without a `.exe`/`.cmd`/`.bat`/`.ps1`/`.js` suffix and
/// reduced to its path basename, mapped to Herdr's canonical agent label (`agent_label`).
pub fn herdr_agent_label(name: &str) -> Option<&'static str> {
    let mut n = name.trim().to_lowercase();
    for suffix in [".exe", ".cmd", ".bat", ".ps1", ".js"] {
        if n.ends_with(suffix) {
            n.truncate(n.len() - suffix.len());
            break;
        }
    }
    let base = n
        .rsplit(['/', '\\'])
        .find(|c| !c.is_empty())
        .unwrap_or(n.as_str());
    Some(match base {
        "pi" => "pi",
        "claude" | "claude-code" => "claude",
        "codex" => "codex",
        "gemini" => "gemini",
        "cursor" | "cursor-agent" => "cursor",
        "devin" | "devin-cli" | "devin cli" => "devin",
        "agy" | "antigravity" | "antigravity-cli" => "agy",
        "cline" | ".cline" => "cline",
        "omp" | "oh-my-pi" => "omp",
        "mastracode" | "mastra-code" | "mastra code" => "mastracode",
        "opencode" | "opencode2" | "open-code" => "opencode",
        "copilot" | "github-copilot" | "ghcs" => "copilot",
        "kimi" | "kimi-code" | "kimi code" => "kimi",
        "kiro" | "kiro-cli" => "kiro",
        "droid" => "droid",
        "amp" | "amp-local" => "amp",
        "grok" | "grok-build" => "grok",
        "hermes" | "hermes-agent" => "hermes",
        "kilo" | "kilo-code" | "kilo code" => "kilo",
        "qodercli" | "qoderclicn" | "qoder" | "qodercn" => "qodercli",
        "qwen" | "qwen-code" | "qwen code" => "qwen",
        "letta" | "letta-code" | "letta code" => "letta",
        "maki" => "maki",
        "muse" | "muse-code" | "muse-cli" => "muse",
        b if b
            .strip_prefix("muse-bin-")
            .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit())) =>
        {
            "muse"
        }
        _ => return None,
    })
}

/// The agent label of one foreground process (Herdr's `normalized_process_name` followed by
/// `identify_agent`, simplified): its `argv0` or name; for a runtime or shell (`node`, `bun`,
/// `deno`, `python*`, `sh`, `bash`, `zsh`, `fish`), the first script argument; else the
/// basename of `argv[0]`.
fn process_agent(p: &serde_json::Value) -> Option<&'static str> {
    let name = p["name"].as_str().unwrap_or_default();
    let effective = p["argv0"].as_str().unwrap_or(name);
    if let Some(l) = herdr_agent_label(effective) {
        return Some(l);
    }
    let argv: Vec<&str> = p["argv"]
        .as_array()
        .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    let runtime = effective.to_lowercase();
    let runtime = runtime.rsplit('/').next().unwrap_or_default();
    let wrapper = matches!(
        runtime,
        "node" | "bun" | "deno" | "sh" | "bash" | "zsh" | "fish"
    ) || runtime.starts_with("python");
    if wrapper
        && let Some(script) = argv.iter().skip(1).find(|a| !a.starts_with('-'))
        && let Some(l) = herdr_agent_label(script)
    {
        return Some(l);
    }
    argv.first().and_then(|a0| herdr_agent_label(a0))
}

/// Herdr's `identify_agent_in_job` (`src/detect/mod.rs`): the process group leader's agent when
/// it is one, else the first member that is an agent. `procs` has the shape of Herdr's
/// `foreground_processes` (`pid`, `name`, `argv0`, `argv`).
pub fn job_agent(pgid: u32, procs: &[serde_json::Value]) -> Option<&'static str> {
    procs
        .iter()
        .find(|p| p["pid"].as_u64() == Some(u64::from(pgid)))
        .and_then(process_agent)
        .or_else(|| procs.iter().find_map(process_agent))
}

/// Herdr wait targets → Vibeke `agent.wait` conditions.
pub fn wait_conditions(herdr_status: &str) -> &'static [&'static str] {
    match herdr_status {
        "idle" => &["idle"],
        "done" => &["done"],
        "working" => &["working"],
        "blocked" => &["needs_approval", "needs_answer"],
        "exited" | "unknown" => &["exited"],
        _ => &[],
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
        assert_eq!(vibeke_harness("oh-my-pi"), "omp");
        assert_eq!(vibeke_harness("codex"), "codex");
        assert_eq!(
            wait_conditions("blocked"),
            &["needs_approval", "needs_answer"]
        );
        assert!(wait_conditions("nope").is_empty());
    }

    #[test]
    fn foreground_agents() {
        use serde_json::json;
        assert_eq!(herdr_agent_label("/usr/local/bin/Claude"), Some("claude"));
        assert_eq!(herdr_agent_label("codex.exe"), Some("codex"));
        assert_eq!(herdr_agent_label("muse-bin-0.1.0"), Some("muse"));
        assert_eq!(herdr_agent_label("bash"), None);
        // A pane that runs only its shell hosts no agent.
        let shell = [json!({"pid": 10, "name": "bash", "argv": ["-bash"]})];
        assert_eq!(job_agent(10, &shell), None);
        let claude = [json!({"pid": 11, "name": "claude", "argv": ["claude"]})];
        assert_eq!(job_agent(11, &claude), Some("claude"));
        // A node shim runs the agent's script.
        let shim =
            [json!({"pid": 12, "name": "node", "argv": ["node", "/opt/bin/codex.js", "--x"]})];
        assert_eq!(job_agent(12, &shim), Some("codex"));
    }

    #[test]
    fn official_sources() {
        assert!(official_agent_source("herdr:claude", "claude"));
        assert!(official_agent_source("herdr:antigravity_cli", "agy"));
        assert!(!official_agent_source("herdr:claude", "codex"));
        assert!(!official_agent_source("my-hook", "claude"));
    }
}
