//! Best-of-N task families (05 §12).
//!
//! `vibeke task new "make the import 3x faster" --agents claude:2,codex:1` creates a family:
//! family handle `k7` and children `k7.1 .. k7.3`, each its own task (worktree, branch, port
//! range) from the same base, all given the same prompt plus an optional per-run suffix
//! (`tasks.best_of_n.suffix`). This module holds the pure parts: parsing the agent list,
//! planning the children, building prompts, the family record and its state machine, collecting
//! a comparable report per child, running the declared check, ranking and rendering. The server
//! (`orch_family.rs`) creates the tasks through `task.create` and feeds the reports.

use crate::{Error, Result, gitx, now_ms};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One entry of `--agents`: a harness and how many runs of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpec {
    pub harness: String,
    pub count: u32,
}

fn valid_harness(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 40
        && h.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Parse `claude:2,codex:1` (also `claude*2`, a bare `claude` is one run). Duplicate harnesses
/// add up. At most `max_children` runs in total, at least two runs overall are not required
/// (a family of one is allowed, it is just a task with a family handle).
pub fn parse_agents(s: &str, max_children: u32) -> Result<Vec<AgentSpec>> {
    let mut out: Vec<AgentSpec> = vec![];
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (h, n) = match part.split_once([':', '*']) {
            Some((h, n)) => {
                let n: u32 = n.trim().parse().map_err(|_| {
                    Error::invalid(format!(
                        "`{part}`: the count after the harness must be a number"
                    ))
                })?;
                (h.trim(), n)
            }
            None => (part, 1),
        };
        if !valid_harness(h) {
            return Err(Error::invalid(format!("`{part}`: not a harness id")));
        }
        if n == 0 {
            return Err(Error::invalid(format!(
                "`{part}`: the count must be at least 1"
            )));
        }
        match out.iter_mut().find(|a| a.harness == h) {
            Some(a) => a.count += n,
            None => out.push(AgentSpec {
                harness: h.to_string(),
                count: n,
            }),
        }
    }
    if out.is_empty() {
        return Err(Error::invalid(
            "no agents given (for example claude:2,codex:1)",
        ));
    }
    let total: u32 = out.iter().map(|a| a.count).sum();
    if total > max_children.max(1) {
        return Err(Error::invalid(format!(
            "{total} runs requested, the limit is {} (orchestrate.best_of_n.max_children)",
            max_children.max(1)
        )));
    }
    Ok(out)
}

/// One planned child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildPlan {
    /// 1-based position in the family (`k7.<index>`).
    pub index: u32,
    pub harness: String,
    /// 1-based position among the runs of the same harness.
    pub ordinal: u32,
}

pub fn plan_children(specs: &[AgentSpec]) -> Vec<ChildPlan> {
    let mut v = vec![];
    for a in specs {
        for ordinal in 1..=a.count {
            v.push(ChildPlan {
                index: v.len() as u32 + 1,
                harness: a.harness.clone(),
                ordinal,
            });
        }
    }
    v
}

/// `k7` + 2 gives `k7.2`.
pub fn child_handle(parent: &str, index: u32) -> String {
    format!("{parent}.{index}")
}

/// `k7.2` gives `("k7", 2)`.
pub fn parse_child_handle(h: &str) -> Option<(&str, u32)> {
    let (p, n) = h.rsplit_once('.')?;
    let n: u32 = n.parse().ok()?;
    (n >= 1 && !p.is_empty() && !p.contains('.')).then_some((p, n))
}

/// The prompt one child gets: the shared prompt plus the suffix. In the suffix `{n}` is the
/// child's index, `{total}` the family size, `{harness}` and `{ordinal}` as planned. `None`
/// when there is nothing to send (neither a prompt nor a suffix).
pub fn child_prompt(
    prompt: Option<&str>,
    suffix: &str,
    child: &ChildPlan,
    total: u32,
) -> Option<String> {
    let suffix = suffix
        .replace("{n}", &child.index.to_string())
        .replace("{total}", &total.to_string())
        .replace("{harness}", &child.harness)
        .replace("{ordinal}", &child.ordinal.to_string());
    let suffix = suffix.trim();
    match (
        prompt.map(str::trim).filter(|p| !p.is_empty()),
        suffix.is_empty(),
    ) {
        (Some(p), true) => Some(p.to_string()),
        (Some(p), false) => Some(format!("{p}\n\n{suffix}")),
        (None, false) => Some(suffix.to_string()),
        (None, true) => None,
    }
}

pub fn child_title(title: &str, child: &ChildPlan) -> String {
    format!("{title} ({} #{})", child.harness, child.index)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FamilyState {
    #[default]
    Running,
    /// A child was picked; the others may be discarded.
    Picked,
    /// Every child was discarded without a pick.
    Discarded,
}

impl FamilyState {
    pub fn as_str(self) -> &'static str {
        match self {
            FamilyState::Running => "running",
            FamilyState::Picked => "picked",
            FamilyState::Discarded => "discarded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Child {
    /// `k7.2`.
    pub handle: String,
    pub task: String,
    pub harness: String,
    pub index: u32,
    pub ordinal: u32,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub discarded: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Family {
    /// The family handle (`k7`).
    pub id: String,
    pub title: String,
    pub repo: String,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub suffix: String,
    pub created_at_ms: i64,
    pub children: Vec<Child>,
    #[serde(default)]
    pub state: FamilyState,
    #[serde(default)]
    pub picked: Option<String>,
    /// Check command resolved at creation (request, config fallback or a trusted repo file).
    #[serde(default)]
    pub check_command: Option<String>,
    /// The latest check outcome per child handle.
    #[serde(default)]
    pub checks: BTreeMap<String, StoredCheck>,
}

/// A check outcome with the revision it ran against (a different head or a changed dirty
/// state makes it stale).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredCheck {
    pub outcome: CheckOutcome,
    pub head: Option<String>,
    pub dirty: bool,
    pub at_ms: i64,
}

impl Family {
    pub fn child(&self, handle_or_task: &str) -> Option<&Child> {
        self.children
            .iter()
            .find(|c| c.handle == handle_or_task || c.task == handle_or_task)
    }

    /// Mark `child` as the winner. Only a running family can pick, once.
    pub fn pick(&mut self, child: &str) -> Result<()> {
        if self.state != FamilyState::Running {
            return Err(Error::Refused(format!(
                "family {} is already {}",
                self.id,
                self.state.as_str()
            )));
        }
        let handle = self
            .child(child)
            .ok_or_else(|| Error::invalid(format!("{child} is not a child of {}", self.id)))?
            .handle
            .clone();
        self.picked = Some(handle);
        self.state = FamilyState::Picked;
        Ok(())
    }

    /// The children that lose a pick (and are not already discarded).
    pub fn losers(&self) -> Vec<&Child> {
        match &self.picked {
            Some(p) => self
                .children
                .iter()
                .filter(|c| &c.handle != p && !c.discarded)
                .collect(),
            None => vec![],
        }
    }

    pub fn mark_discarded(&mut self, handle: &str) {
        if let Some(c) = self.children.iter_mut().find(|c| c.handle == handle) {
            c.discarded = true;
        }
        if self.picked.is_none() && self.children.iter().all(|c| c.discarded) {
            self.state = FamilyState::Discarded;
        }
    }
}

/// What the server knows about a child when a comparison is requested.
#[derive(Debug, Clone)]
pub struct ChildInput {
    pub handle: String,
    pub task: String,
    pub harness: String,
    pub worktree: PathBuf,
    pub branch: Option<String>,
    pub base_ref: Option<String>,
    /// Task status or the run's execution state, shown as is.
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    pub path: String,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DiffSummary {
    pub files: Vec<FileStat>,
    pub additions: u32,
    pub deletions: u32,
    pub commits: u32,
    pub dirty: bool,
    pub head: Option<String>,
}

fn untracked_lines(root: &Path, rel: &str) -> u32 {
    let p = root.join(rel);
    match std::fs::metadata(&p) {
        Ok(m) if m.is_file() && m.len() <= 1_000_000 => std::fs::read(&p)
            .ok()
            .filter(|b| !b.contains(&0))
            .map(|b| b.iter().filter(|&&c| c == b'\n').count() as u32)
            .unwrap_or(0),
        _ => 0,
    }
}

/// What `worktree` adds on top of `base`: committed work since the merge base plus staged,
/// unstaged and untracked changes.
pub fn diff_summary(worktree: &Path, base: Option<&str>) -> Result<DiffSummary> {
    let from = match base {
        Some(b) => {
            let b = gitx::rev_parse(worktree, b).unwrap_or_else(|_| b.to_string());
            gitx::run(worktree, &["merge-base", &b, "HEAD"]).unwrap_or(b)
        }
        None => "HEAD".to_string(),
    };
    let numstat = gitx::run(worktree, &["diff", "--numstat", "--no-renames", &from])?;
    let mut files: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    for l in numstat.lines() {
        let mut it = l.splitn(3, '\t');
        let (a, d, path) = (it.next(), it.next(), it.next());
        if let Some(path) = path {
            files.insert(
                path.to_string(),
                (
                    a.and_then(|x| x.parse().ok()).unwrap_or(0),
                    d.and_then(|x| x.parse().ok()).unwrap_or(0),
                ),
            );
        }
    }
    let untracked = gitx::run_bytes(
        worktree,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )?;
    let untracked = gitx::split_nul(&untracked);
    for u in &untracked {
        files
            .entry(u.clone())
            .or_insert((untracked_lines(worktree, u), 0));
    }
    let status = gitx::run(worktree, &["status", "--porcelain"])?;
    let commits = gitx::run(worktree, &["rev-list", "--count", &format!("{from}..HEAD")])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let files: Vec<FileStat> = files
        .into_iter()
        .map(|(path, (additions, deletions))| FileStat {
            path,
            additions,
            deletions,
        })
        .collect();
    Ok(DiffSummary {
        additions: files.iter().map(|f| f.additions).sum(),
        deletions: files.iter().map(|f| f.deletions).sum(),
        files,
        commits,
        dirty: !status.trim().is_empty(),
        head: gitx::rev_parse(worktree, "HEAD").ok(),
    })
}

/// `git diff a..b` numbers between two branches of one repository (pair comparison).
pub fn pairwise(repo: &Path, a: &str, b: &str) -> Result<DiffSummary> {
    let range = format!("{a}..{b}");
    let numstat = gitx::run(repo, &["diff", "--numstat", "--no-renames", &range])?;
    let mut files = vec![];
    for l in numstat.lines() {
        let mut it = l.splitn(3, '\t');
        let (x, y, p) = (it.next(), it.next(), it.next());
        if let Some(p) = p {
            files.push(FileStat {
                path: p.to_string(),
                additions: x.and_then(|v| v.parse().ok()).unwrap_or(0),
                deletions: y.and_then(|v| v.parse().ok()).unwrap_or(0),
            });
        }
    }
    Ok(DiffSummary {
        additions: files.iter().map(|f| f.additions).sum(),
        deletions: files.iter().map(|f| f.deletions).sum(),
        files,
        commits: 0,
        dirty: false,
        head: None,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckOutcome {
    pub command: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u64,
    /// The last few KiB of combined output.
    pub tail: String,
}

const TAIL_BYTES: usize = 4096;

/// Run `command` (`sh -c`) in `cwd` with a timeout. Output is read to the end on threads; the
/// whole process group is killed on timeout.
pub fn run_check(cwd: &Path, command: &str, timeout: Duration) -> CheckOutcome {
    use std::os::unix::process::CommandExt;
    let start = Instant::now();
    let fail = |msg: String, start: Instant| CheckOutcome {
        command: command.to_string(),
        ok: false,
        exit_code: None,
        timed_out: false,
        duration_ms: start.elapsed().as_millis() as u64,
        tail: msg,
    };
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail(format!("could not start: {e}"), start),
    };
    let pid = child.id() as libc::pid_t;
    let mut readers = vec![];
    if let Some(mut o) = child.stdout.take() {
        readers.push(std::thread::spawn(move || {
            let mut b = vec![];
            let _ = o.read_to_end(&mut b);
            b
        }));
    }
    if let Some(mut e) = child.stderr.take() {
        readers.push(std::thread::spawn(move || {
            let mut b = vec![];
            let _ = e.read_to_end(&mut b);
            b
        }));
    }
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    timed_out = true;
                    // SAFETY: plain kill(2) on the process group of a child we spawned.
                    unsafe { libc::kill(-pid, libc::SIGKILL) };
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(_) => break None,
        }
    };
    let mut out = vec![];
    for r in readers {
        if let Ok(b) = r.join() {
            out.extend(b);
        }
    }
    let skip = out.len().saturating_sub(TAIL_BYTES);
    let tail = String::from_utf8_lossy(&out[skip..]).into_owned();
    CheckOutcome {
        command: command.to_string(),
        ok: status.is_some_and(|s| s.success()) && !timed_out,
        exit_code: status.and_then(|s| s.code()),
        timed_out,
        duration_ms: start.elapsed().as_millis() as u64,
        tail,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChildReport {
    pub handle: String,
    pub task: String,
    pub harness: String,
    pub branch: Option<String>,
    pub state: String,
    pub summary: DiffSummary,
    pub check: Option<CheckOutcome>,
    /// The check ran against a different revision than the one reported.
    #[serde(default)]
    pub check_stale: bool,
    /// Files this child also shares with at least one sibling.
    pub shared_files: u32,
}

/// Attach the stored check to a report; a check from another revision is kept for display but
/// flagged stale (and ignored by [`rank`]).
pub fn attach_check(r: &mut ChildReport, stored: Option<&StoredCheck>) {
    match stored {
        Some(c) => {
            r.check = Some(c.outcome.clone());
            r.check_stale = c.head != r.summary.head || c.dirty != r.summary.dirty;
        }
        None => {
            r.check = None;
            r.check_stale = false;
        }
    }
}

/// Collect a child's report. A broken worktree yields a report with an empty summary and the
/// error as its state suffix instead of failing the whole comparison.
pub fn collect_report(input: &ChildInput, check: Option<CheckOutcome>) -> ChildReport {
    let (summary, state) = match diff_summary(&input.worktree, input.base_ref.as_deref()) {
        Ok(s) => (s, input.state.clone()),
        Err(e) => (
            DiffSummary::default(),
            format!("{} (diff failed: {e})", input.state),
        ),
    };
    ChildReport {
        handle: input.handle.clone(),
        task: input.task.clone(),
        harness: input.harness.clone(),
        branch: input.branch.clone(),
        state,
        summary,
        check,
        check_stale: false,
        shared_files: 0,
    }
}

/// Fill `shared_files` on each report.
pub fn mark_shared(reports: &mut [ChildReport]) {
    let sets: Vec<BTreeSet<&str>> = reports
        .iter()
        .map(|r| r.summary.files.iter().map(|f| f.path.as_str()).collect())
        .collect();
    let shared: Vec<u32> = sets
        .iter()
        .enumerate()
        .map(|(i, mine)| {
            mine.iter()
                .filter(|f| {
                    sets.iter()
                        .enumerate()
                        .any(|(j, o)| j != i && o.contains(*f))
                })
                .count() as u32
        })
        .collect();
    for (r, s) in reports.iter_mut().zip(shared) {
        r.shared_files = s;
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ranked {
    pub handle: String,
    pub score: i64,
    pub reasons: Vec<String>,
}

/// Deterministic ranking, best first. A passing check dominates, then a smaller diff; a child
/// that produced nothing or whose check failed sinks. This is a sort aid for the comparison
/// view, never an acceptance decision.
pub fn rank(reports: &[ChildReport]) -> Vec<Ranked> {
    let mut v: Vec<(usize, Ranked)> = reports
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let mut score: i64 = 0;
            let mut reasons = vec![];
            match r.check.as_ref().filter(|_| !r.check_stale) {
                Some(c) if c.ok => {
                    score += 1000;
                    reasons.push("check passed".to_string());
                }
                Some(c) if c.timed_out => {
                    score -= 1000;
                    reasons.push("check timed out".into());
                }
                Some(_) => {
                    score -= 1000;
                    reasons.push("check failed".into());
                }
                None if r.check_stale => reasons.push("check is stale".into()),
                None => reasons.push("no check".into()),
            }
            let size = i64::from(r.summary.additions) + i64::from(r.summary.deletions);
            if r.summary.files.is_empty() && r.summary.commits == 0 {
                score -= 2000;
                reasons.push("no changes".into());
            } else {
                score -= size.min(900) / 3;
                reasons.push(format!(
                    "+{} -{} in {} file(s)",
                    r.summary.additions,
                    r.summary.deletions,
                    r.summary.files.len()
                ));
            }
            if r.state.contains("failed") || r.state == "error" {
                score -= 500;
                reasons.push(format!("state {}", r.state));
            }
            (
                i,
                Ranked {
                    handle: r.handle.clone(),
                    score,
                    reasons,
                },
            )
        })
        .collect();
    v.sort_by(|a, b| b.1.score.cmp(&a.1.score).then(a.0.cmp(&b.0)));
    v.into_iter().map(|(_, r)| r).collect()
}

/// The plain-text table `vibeke task compare k7` prints.
pub fn render_compare(family: &str, reports: &[ChildReport], ranked: &[Ranked]) -> String {
    let mut s = format!("family {family}: {} child(ren)\n", reports.len());
    s.push_str(&format!(
        "{:<8} {:<10} {:<12} {:>6} {:>6} {:>6} {:<8} {:>5}\n",
        "CHILD", "HARNESS", "STATE", "FILES", "+ADD", "-DEL", "CHECK", "RANK"
    ));
    for r in reports {
        let pos = ranked
            .iter()
            .position(|k| k.handle == r.handle)
            .map(|i| (i + 1).to_string())
            .unwrap_or_else(|| "-".into());
        let check = match &r.check {
            Some(c) if r.check_stale => {
                if c.ok {
                    "pass*"
                } else {
                    "FAIL*"
                }
            }
            Some(c) if c.ok => "pass",
            Some(c) if c.timed_out => "timeout",
            Some(_) => "FAIL",
            None => "-",
        };
        s.push_str(&format!(
            "{:<8} {:<10} {:<12} {:>6} {:>6} {:>6} {:<8} {:>5}\n",
            r.handle,
            r.harness,
            r.state.chars().take(12).collect::<String>(),
            r.summary.files.len(),
            r.summary.additions,
            r.summary.deletions,
            check,
            pos
        ));
    }
    if let Some(first) = reports.first() {
        for other in reports.iter().skip(1) {
            if let (Some(a), Some(b)) = (&first.branch, &other.branch) {
                s.push_str(&format!(
                    "git diff {a}..{b}   # {} vs {}\n",
                    first.handle, other.handle
                ));
            }
        }
    }
    s
}

/// Timestamp helper for family records created by the server.
pub fn new_family(
    id: &str,
    title: &str,
    repo: &str,
    base: Option<&str>,
    prompt: Option<&str>,
    suffix: &str,
    check_command: Option<&str>,
) -> Family {
    Family {
        id: id.to_string(),
        title: title.to_string(),
        repo: repo.to_string(),
        base: base.map(str::to_string),
        prompt: prompt.map(str::to_string),
        suffix: suffix.to_string(),
        created_at_ms: now_ms(),
        children: vec![],
        state: FamilyState::Running,
        picked: None,
        check_command: check_command.map(str::to_string),
        checks: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitx::testutil::*;

    #[test]
    fn parses_agent_lists() {
        let v = parse_agents("claude:2,codex:1", 8).unwrap();
        assert_eq!(
            v,
            vec![
                AgentSpec {
                    harness: "claude".into(),
                    count: 2
                },
                AgentSpec {
                    harness: "codex".into(),
                    count: 1
                }
            ]
        );
        assert_eq!(parse_agents("pi", 8).unwrap()[0].count, 1);
        assert_eq!(parse_agents("claude*3", 8).unwrap()[0].count, 3);
        // duplicates add up
        let v = parse_agents("claude:1, codex:1, claude:2", 8).unwrap();
        assert_eq!(v[0].count, 3);
        assert_eq!(v.len(), 2);
        assert!(parse_agents("", 8).is_err());
        assert!(parse_agents("claude:0", 8).is_err());
        assert!(parse_agents("claude:x", 8).is_err());
        assert!(parse_agents("cl aude:1", 8).is_err());
        assert!(parse_agents("claude:9", 8).is_err());
    }

    #[test]
    fn plans_children_and_handles() {
        let p = plan_children(&parse_agents("claude:2,codex:1", 8).unwrap());
        assert_eq!(p.len(), 3);
        assert_eq!(
            (p[0].index, p[0].harness.as_str(), p[0].ordinal),
            (1, "claude", 1)
        );
        assert_eq!((p[1].index, p[1].ordinal), (2, 2));
        assert_eq!(
            (p[2].index, p[2].harness.as_str(), p[2].ordinal),
            (3, "codex", 1)
        );
        assert_eq!(child_handle("k7", 3), "k7.3");
        assert_eq!(parse_child_handle("k7.3"), Some(("k7", 3)));
        assert_eq!(parse_child_handle("k7"), None);
        assert_eq!(parse_child_handle("k7.x"), None);
        assert_eq!(parse_child_handle("k7.0"), None);
        assert_eq!(parse_child_handle("a.b.1"), None);
    }

    #[test]
    fn prompts_get_the_suffix() {
        let c = ChildPlan {
            index: 2,
            harness: "codex".into(),
            ordinal: 1,
        };
        assert_eq!(
            child_prompt(Some("do it"), "", &c, 3).as_deref(),
            Some("do it")
        );
        assert_eq!(
            child_prompt(
                Some("do it"),
                "Attempt {n} of {total} with {harness}.",
                &c,
                3
            )
            .as_deref(),
            Some("do it\n\nAttempt 2 of 3 with codex.")
        );
        assert_eq!(child_prompt(None, "only", &c, 3).as_deref(), Some("only"));
        assert_eq!(child_prompt(None, "  ", &c, 3), None);
        assert_eq!(child_prompt(Some("  "), "", &c, 3), None);
        assert_eq!(
            child_title("Speed up import", &c),
            "Speed up import (codex #2)"
        );
    }

    fn family() -> Family {
        let mut f = new_family("k7", "t", "/r", Some("main"), Some("p"), "", None);
        for (i, h) in ["claude", "claude", "codex"].iter().enumerate() {
            f.children.push(Child {
                handle: child_handle("k7", i as u32 + 1),
                task: format!("task{i}"),
                harness: h.to_string(),
                index: i as u32 + 1,
                ordinal: 1,
                run: None,
                discarded: false,
            });
        }
        f
    }

    #[test]
    fn family_pick_state_machine() {
        let mut f = family();
        assert!(f.pick("k7.9").is_err());
        f.pick("task1").unwrap();
        assert_eq!(f.picked.as_deref(), Some("k7.2"));
        assert_eq!(f.state, FamilyState::Picked);
        assert!(f.pick("k7.1").is_err(), "only once");
        let losers: Vec<_> = f.losers().iter().map(|c| c.handle.clone()).collect();
        assert_eq!(losers, vec!["k7.1", "k7.3"]);
        f.mark_discarded("k7.1");
        assert_eq!(f.losers().len(), 1);
        // serde round trip
        let j = serde_json::to_string(&f).unwrap();
        assert_eq!(serde_json::from_str::<Family>(&j).unwrap(), f);
        // discarding everything without a pick ends the family
        let mut g = family();
        for h in ["k7.1", "k7.2", "k7.3"] {
            g.mark_discarded(h);
        }
        assert_eq!(g.state, FamilyState::Discarded);
    }

    #[test]
    fn diff_summary_counts_commits_dirty_and_untracked() {
        let r = repo(&[("a.txt", "one\n"), ("b.txt", "x\ny\n")]);
        let wt = worktree(&r, "t/one");
        write(&wt, "a.txt", "one\ntwo\nthree\n");
        commit_all(&wt, "edit a");
        write(&wt, "b.txt", "x\n");
        write(&wt, "new.txt", "n1\nn2\n");
        let s = diff_summary(&wt, Some("main")).unwrap();
        assert_eq!(s.commits, 1);
        assert!(s.dirty);
        let by: BTreeMap<_, _> = s
            .files
            .iter()
            .map(|f| (f.path.as_str(), (f.additions, f.deletions)))
            .collect();
        assert_eq!(by["a.txt"], (2, 0));
        assert_eq!(by["b.txt"], (0, 1));
        assert_eq!(by["new.txt"], (2, 0));
        assert_eq!(s.additions, 4);
        assert_eq!(s.deletions, 1);
        assert!(s.head.is_some());
    }

    #[test]
    fn pairwise_between_branches() {
        let r = repo(&[("a.txt", "one\n")]);
        let w1 = worktree(&r, "t/one");
        let w2 = worktree(&r, "t/two");
        write(&w1, "a.txt", "one\nuno\n");
        commit_all(&w1, "1");
        write(&w2, "a.txt", "one\ndos\ntres\n");
        commit_all(&w2, "2");
        let d = pairwise(&r.root, "t/one", "t/two").unwrap();
        assert_eq!(d.files.len(), 1);
        assert_eq!((d.additions, d.deletions), (2, 1));
    }

    #[test]
    fn check_runs_with_exit_codes_and_timeouts() {
        let t = tempfile::tempdir().unwrap();
        let ok = run_check(t.path(), "echo hello && exit 0", Duration::from_secs(10));
        assert!(ok.ok);
        assert_eq!(ok.exit_code, Some(0));
        assert!(ok.tail.contains("hello"));
        let bad = run_check(t.path(), "echo boom >&2; exit 3", Duration::from_secs(10));
        assert!(!bad.ok);
        assert_eq!(bad.exit_code, Some(3));
        assert!(bad.tail.contains("boom"));
        let slow = run_check(t.path(), "sleep 30", Duration::from_millis(200));
        assert!(slow.timed_out);
        assert!(!slow.ok);
        assert!(slow.duration_ms < 5000);
    }

    fn rep(
        h: &str,
        add: u32,
        del: u32,
        check: Option<bool>,
        state: &str,
        files: &[&str],
    ) -> ChildReport {
        ChildReport {
            handle: h.into(),
            task: h.into(),
            harness: "claude".into(),
            branch: Some(format!("b/{h}")),
            state: state.into(),
            summary: DiffSummary {
                files: files
                    .iter()
                    .map(|p| FileStat {
                        path: p.to_string(),
                        additions: add,
                        deletions: del,
                    })
                    .collect(),
                additions: add * files.len() as u32,
                deletions: del * files.len() as u32,
                commits: 1,
                dirty: false,
                head: None,
            },
            check: check.map(|ok| CheckOutcome {
                command: "t".into(),
                ok,
                exit_code: Some(if ok { 0 } else { 1 }),
                timed_out: false,
                duration_ms: 1,
                tail: String::new(),
            }),
            check_stale: false,
            shared_files: 0,
        }
    }

    #[test]
    fn ranking_prefers_passing_checks_then_small_diffs() {
        let mut rs = vec![
            rep("k7.1", 50, 5, Some(false), "idle", &["a"]),
            rep("k7.2", 40, 5, Some(true), "idle", &["a", "b"]),
            rep("k7.3", 3, 1, Some(true), "idle", &["a"]),
            rep("k7.4", 0, 0, None, "idle", &[]),
        ];
        rs[3].summary.commits = 0;
        mark_shared(&mut rs);
        assert_eq!(rs[1].shared_files, 1);
        assert_eq!(rs[3].shared_files, 0);
        let r = rank(&rs);
        let order: Vec<_> = r.iter().map(|x| x.handle.as_str()).collect();
        assert_eq!(order, vec!["k7.3", "k7.2", "k7.1", "k7.4"]);
        assert!(r[3].reasons.iter().any(|x| x == "no changes"));
        let text = render_compare("k7", &rs, &r);
        assert!(text.contains("k7.3"));
        assert!(text.contains("FAIL"));
        assert!(text.contains("git diff b/k7.1..b/k7.2"));
    }

    #[test]
    fn stale_checks_are_flagged_and_not_ranked() {
        let mut a = rep("k7.1", 3, 1, None, "idle", &["a"]);
        a.summary.head = Some("h1".into());
        let stored = StoredCheck {
            outcome: CheckOutcome {
                command: "t".into(),
                ok: true,
                exit_code: Some(0),
                timed_out: false,
                duration_ms: 1,
                tail: String::new(),
            },
            head: Some("h1".into()),
            dirty: false,
            at_ms: 1,
        };
        attach_check(&mut a, Some(&stored));
        assert!(a.check.is_some() && !a.check_stale);
        // A new commit makes it stale.
        a.summary.head = Some("h2".into());
        attach_check(&mut a, Some(&stored));
        assert!(a.check_stale);
        let mut b = rep("k7.2", 3, 1, None, "idle", &["b"]);
        b.summary.head = Some("h1".into());
        let r = rank(&[a.clone(), b]);
        assert!(r[0].reasons.iter().chain(r[1].reasons.iter()).any(|x| x == "check is stale"));
        assert!(render_compare("k7", &[a.clone()], &r).contains("pass*"));
        // Dirty state changes also stale it.
        a.summary.head = Some("h1".into());
        a.summary.dirty = true;
        attach_check(&mut a, Some(&stored));
        assert!(a.check_stale);
        attach_check(&mut a, None);
        assert!(a.check.is_none() && !a.check_stale);
    }

    #[test]
    fn collect_report_tolerates_missing_worktree() {
        let rep = collect_report(
            &ChildInput {
                handle: "k7.1".into(),
                task: "t".into(),
                harness: "claude".into(),
                worktree: PathBuf::from("/nonexistent/worktree"),
                branch: None,
                base_ref: None,
                state: "active".into(),
            },
            None,
        );
        assert!(rep.state.contains("diff failed"));
        assert!(rep.summary.files.is_empty());
    }
}
