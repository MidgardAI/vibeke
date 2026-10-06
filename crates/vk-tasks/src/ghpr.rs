//! Pull-request status through the `gh` CLI (05 §13).
//!
//! Only when `gh` is installed *and* authenticated; never prompts (stdin is
//! null, `GH_PROMPT_DISABLED=1`); every call has a timeout; results (including
//! "no PR" and "gh unavailable") are cached for [`PR_CACHE_TTL`].
//!
//! Tests point [`gh_binary`] at a fake with `VIBEKE_GH_BIN=<absolute path>`,
//! honoured only while `VIBEKE_TEST_HOOKS=1`.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const PR_CACHE_TTL: Duration = Duration::from_secs(60);
const GH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    None,
    Pending,
    Passing,
    Failing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrStatus {
    pub number: u64,
    /// `OPEN` | `MERGED` | `CLOSED`
    pub state: String,
    pub is_draft: bool,
    pub review_decision: Option<String>,
    pub checks: ChecksState,
    pub url: String,
    /// Sidebar text: `#123 ✓`, `#123 ✗ checks`, `#123 draft`, `#123 merged`.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrLookup {
    Pr {
        pr: PrStatus,
    },
    /// `gh` answered: this branch has no pull request.
    NoPr,
    /// No status can be shown (not installed, not authenticated, timed out, ...).
    Unavailable {
        reason: String,
    },
}

/// `gh`, or the test override.
pub fn gh_binary() -> PathBuf {
    if std::env::var("VIBEKE_TEST_HOOKS").is_ok_and(|v| v == "1")
        && let Some(p) = std::env::var_os("VIBEKE_GH_BIN")
    {
        let p = PathBuf::from(p);
        if p.is_absolute() {
            return p;
        }
    }
    PathBuf::from("gh")
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_gh(cwd: &Path, args: &[&str]) -> Result<Ran, String> {
    let mut child = Command::new(gh_binary())
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GH_SPINNER_DISABLED", "1")
        .env("NO_COLOR", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "gh is not installed".to_string(),
            _ => format!("cannot run gh: {e}"),
        })?;
    fn drain<R: Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut r) = r {
                let _ = r.read_to_string(&mut s);
            }
            s
        })
    }
    let (out, err) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if start.elapsed() >= GH_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("gh timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(e) => return Err(format!("gh failed: {e}")),
        }
    };
    Ok(Ran {
        code: status.code(),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// Is `gh` installed and logged in? (`gh auth status` exit code; no prompt.)
pub fn gh_ready(cwd: &Path) -> Result<(), String> {
    let r = run_gh(cwd, &["auth", "status"])?;
    if r.code == Some(0) {
        Ok(())
    } else {
        Err("gh is not authenticated (run `gh auth login`)".into())
    }
}

/// Parse `gh pr view --json number,state,isDraft,reviewDecision,statusCheckRollup,url`.
pub fn parse_pr(json: &str) -> Option<PrStatus> {
    let v: Value = serde_json::from_str(json).ok()?;
    let number = v.get("number")?.as_u64()?;
    let state = v.get("state")?.as_str()?.to_ascii_uppercase();
    let is_draft = v.get("isDraft").and_then(Value::as_bool).unwrap_or(false);
    let review_decision = v
        .get("reviewDecision")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let url = v
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let checks = checks_state(v.get("statusCheckRollup"));
    let label = match (state.as_str(), is_draft, checks) {
        ("MERGED", _, _) => format!("#{number} merged"),
        ("CLOSED", _, _) => format!("#{number} closed"),
        (_, true, _) => format!("#{number} draft"),
        (_, _, ChecksState::Failing) => format!("#{number} ✗ checks"),
        (_, _, ChecksState::Passing) => format!("#{number} ✓"),
        (_, _, ChecksState::Pending) => format!("#{number} …"),
        _ => format!("#{number}"),
    };
    Some(PrStatus {
        number,
        state,
        is_draft,
        review_decision,
        checks,
        url,
        label,
    })
}

fn checks_state(rollup: Option<&Value>) -> ChecksState {
    let Some(items) = rollup.and_then(Value::as_array).filter(|a| !a.is_empty()) else {
        return ChecksState::None;
    };
    let up = |it: &Value, k: &str| {
        it.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_uppercase()
    };
    let (mut failing, mut pending) = (false, false);
    for it in items {
        // CheckRun: status + conclusion. StatusContext: state.
        let (status, conclusion, state) = (up(it, "status"), up(it, "conclusion"), up(it, "state"));
        if matches!(
            conclusion.as_str(),
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
        ) || matches!(state.as_str(), "FAILURE" | "ERROR")
        {
            failing = true;
        } else if (!status.is_empty() && status != "COMPLETED")
            || matches!(state.as_str(), "PENDING" | "EXPECTED")
        {
            pending = true;
        }
    }
    if failing {
        ChecksState::Failing
    } else if pending {
        ChecksState::Pending
    } else {
        ChecksState::Passing
    }
}

/// Fetch the PR of the branch checked out in `worktree`, uncached.
pub fn fetch_pr(worktree: &Path) -> PrLookup {
    if crate::is_contained(worktree) {
        // A box can rewrite this checkout's git config; gh would run git in it.
        return PrLookup::Unavailable {
            reason: "sandboxed checkout".into(),
        };
    }
    if let Err(reason) = gh_ready(worktree) {
        return PrLookup::Unavailable { reason };
    }
    let r = match run_gh(
        worktree,
        &[
            "pr",
            "view",
            "--json",
            "number,state,isDraft,reviewDecision,statusCheckRollup,url",
        ],
    ) {
        Ok(r) => r,
        Err(reason) => return PrLookup::Unavailable { reason },
    };
    if r.code == Some(0) {
        return match parse_pr(&r.stdout) {
            Some(pr) => PrLookup::Pr { pr },
            None => PrLookup::Unavailable {
                reason: "unreadable gh output".into(),
            },
        };
    }
    if r.stderr
        .to_ascii_lowercase()
        .contains("no pull requests found")
    {
        PrLookup::NoPr
    } else {
        PrLookup::Unavailable {
            reason: r.stderr.lines().next().unwrap_or("gh failed").to_string(),
        }
    }
}

/// JSON fields requested for PR evidence (15 §6.4): identity, target branch, head revision,
/// draft state, review decision and the checks rollup.
pub const PR_EVIDENCE_FIELDS: &str =
    "number,url,state,isDraft,baseRefName,headRefName,headRefOid,reviewDecision,statusCheckRollup";

/// Outcome of [`fetch_pr_evidence_json`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrJson {
    /// Raw `gh pr view --json PR_EVIDENCE_FIELDS` output.
    Found(String),
    /// `gh` answered: no pull request for that reference.
    NoPr,
    /// No answer (not installed, not authenticated, timed out, sandboxed checkout, bad
    /// reference ...). The caller records an unknown observation, never a pass or a failure.
    Unavailable(String),
}

/// A PR reference `gh pr view` may take: a number, a URL or a branch name. Anything that could
/// be read as an option is refused.
pub fn valid_pr_ref(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 512
        && !r.starts_with('-')
        && !r.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Fetch the PR identity and head facts for `pr` (or, when `None`, the PR of the branch checked
/// out in `worktree`), uncached. Same safety rules as [`fetch_pr`].
pub fn fetch_pr_evidence_json(worktree: &Path, pr: Option<&str>) -> PrJson {
    if let Some(r) = pr
        && !valid_pr_ref(r)
    {
        return PrJson::Unavailable("invalid pull request reference".into());
    }
    if crate::is_contained(worktree) {
        return PrJson::Unavailable("sandboxed checkout".into());
    }
    if let Err(reason) = gh_ready(worktree) {
        return PrJson::Unavailable(reason);
    }
    let mut args = vec!["pr", "view"];
    if let Some(r) = pr {
        args.push(r);
    }
    args.extend(["--json", PR_EVIDENCE_FIELDS]);
    let r = match run_gh(worktree, &args) {
        Ok(r) => r,
        Err(reason) => return PrJson::Unavailable(reason),
    };
    if r.code == Some(0) {
        return PrJson::Found(r.stdout);
    }
    if r.stderr
        .to_ascii_lowercase()
        .contains("no pull requests found")
    {
        PrJson::NoPr
    } else {
        PrJson::Unavailable(r.stderr.lines().next().unwrap_or("gh failed").to_string())
    }
}

/// 60 s cache of PR lookups, keyed by worktree path.
#[derive(Default)]
pub struct PrCache {
    ttl: Option<Duration>,
    map: Mutex<HashMap<PathBuf, (Instant, PrLookup)>>,
}

impl PrCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cache with a custom lifetime (tests).
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl: Some(ttl),
            ..Self::default()
        }
    }

    fn ttl(&self) -> Duration {
        self.ttl.unwrap_or(PR_CACHE_TTL)
    }

    /// The cached answer, if fresh. Never runs `gh`.
    pub fn peek(&self, worktree: &Path) -> Option<PrLookup> {
        let m = self.map.lock().unwrap();
        m.get(worktree)
            .filter(|(at, _)| at.elapsed() < self.ttl())
            .map(|(_, l)| l.clone())
    }

    /// Cached answer, or run `gh` (blocking up to its timeout) and cache it.
    /// `refresh` bypasses a fresh entry.
    pub fn get(&self, worktree: &Path, refresh: bool) -> PrLookup {
        if !refresh && let Some(l) = self.peek(worktree) {
            return l;
        }
        let l = fetch_pr(worktree);
        self.map
            .lock()
            .unwrap()
            .insert(worktree.to_path_buf(), (Instant::now(), l.clone()));
        l
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_follow_the_spec() {
        let j = |state: &str, draft: bool, roll: &str| {
            parse_pr(&format!(
                r#"{{"number":123,"state":"{state}","isDraft":{draft},"reviewDecision":"","url":"https://x/pull/123","statusCheckRollup":{roll}}}"#
            ))
            .unwrap()
        };
        let ok = r#"[{"status":"COMPLETED","conclusion":"SUCCESS"},{"state":"SUCCESS"}]"#;
        let bad = r#"[{"status":"COMPLETED","conclusion":"SUCCESS"},{"status":"COMPLETED","conclusion":"FAILURE"}]"#;
        let pend = r#"[{"status":"IN_PROGRESS","conclusion":""}]"#;
        assert_eq!(j("OPEN", false, ok).label, "#123 ✓");
        assert_eq!(j("OPEN", false, bad).label, "#123 ✗ checks");
        assert_eq!(j("OPEN", false, pend).label, "#123 …");
        assert_eq!(j("OPEN", false, "[]").label, "#123");
        assert_eq!(j("OPEN", true, ok).label, "#123 draft");
        assert_eq!(j("MERGED", false, ok).label, "#123 merged");
        assert_eq!(j("OPEN", false, ok).url, "https://x/pull/123");
        assert!(j("OPEN", false, ok).review_decision.is_none());
        assert!(parse_pr("not json").is_none());
    }

    #[test]
    fn override_is_ignored_without_test_hooks() {
        // The test binary does not set VIBEKE_TEST_HOOKS=1 for this check, so
        // an override alone must not redirect gh.
        if std::env::var("VIBEKE_TEST_HOOKS").is_err() {
            assert_eq!(gh_binary(), PathBuf::from("gh"));
        }
    }
}
