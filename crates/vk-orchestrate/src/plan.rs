//! Goal to plan to tasks (12 "Goal -> plan -> tasks"): a goal is planned into steps, a person
//! approves the plan, and only then does the planner fan the steps out as tasks, routed to a
//! harness by fit, cost and quota. "What happened while you were away" briefings summarize the
//! event log.
//!
//! The planner is a trait. [`HeuristicPlanner`] needs no provider: it reads the structure of
//! the goal text (numbered lists are sequential, bullets are independent). [`ScriptedPlanner`]
//! is the fake used by tests. A planning *agent* (config `backend = "agent"`) is given
//! [`planner_prompt`] and submits JSON that [`parse_plan_json`] validates; that path needs a
//! live harness and is gated by the user. Whatever produced a plan, [`validate`] is the same
//! gate, and **nothing starts before approval** (`planner.approval_required`): editing the
//! plan after approval clears the approval.

use crate::{Error, Result, now_ms};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    #[default]
    Implement,
    Test,
    Docs,
    Research,
    Review,
    Refactor,
    Other,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Implement => "implement",
            StepKind::Test => "test",
            StepKind::Docs => "docs",
            StepKind::Research => "research",
            StepKind::Review => "review",
            StepKind::Refactor => "refactor",
            StepKind::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Quick,
    #[default]
    Minutes,
    Deep,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub title: String,
    pub prompt: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub kind: StepKind,
    #[serde(default)]
    pub effort: Effort,
    /// A harness the plan prefers; routing may override it when it is limited or missing.
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub priority: i32,
    /// Repository-relative globs the step will work in (become claims).
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Plan {
    pub steps: Vec<Step>,
    #[serde(default)]
    pub rationale: String,
    #[serde(default)]
    pub planner: String,
    #[serde(default)]
    pub created_at_ms: i64,
}

/// Problems with a plan, empty when it is acceptable.
pub fn validate(plan: &Plan, max_steps: u32) -> Vec<String> {
    let mut e = vec![];
    if plan.steps.is_empty() {
        e.push("the plan has no steps".to_string());
        return e;
    }
    if plan.steps.len() as u32 > max_steps.max(1) {
        e.push(format!(
            "{} steps, the limit is {} (orchestrate.planner.max_steps)",
            plan.steps.len(),
            max_steps.max(1)
        ));
    }
    let mut ids = BTreeSet::new();
    for s in &plan.steps {
        if s.id.is_empty()
            || !s
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            e.push(format!(
                "step id `{}` must be letters, digits, `-` or `_`",
                s.id
            ));
        }
        if !ids.insert(s.id.as_str()) {
            e.push(format!("duplicate step id `{}`", s.id));
        }
        if s.title.trim().is_empty() {
            e.push(format!("step {} has no title", s.id));
        }
        if s.prompt.trim().is_empty() {
            e.push(format!("step {} has no prompt", s.id));
        }
        if s.prompt.len() > 20_000 {
            e.push(format!("step {} has a prompt over 20000 characters", s.id));
        }
        for p in &s.paths {
            if crate::merge::validate_claim_glob(p).is_err() {
                e.push(format!(
                    "step {}: `{p}` is not a repository-relative glob",
                    s.id
                ));
            }
        }
    }
    for s in &plan.steps {
        for d in &s.depends_on {
            if d == &s.id {
                e.push(format!("step {} depends on itself", s.id));
            } else if !ids.contains(d.as_str()) {
                e.push(format!("step {} depends on unknown step `{d}`", s.id));
            }
        }
    }
    if e.is_empty() && levels(plan).is_err() {
        e.push("the steps' dependencies form a cycle".into());
    }
    e
}

/// Steps grouped by dependency depth; an error for a cycle.
pub fn levels(plan: &Plan) -> Result<Vec<Vec<String>>> {
    let mut remaining: Vec<&Step> = plan.steps.iter().collect();
    let mut done: BTreeSet<&str> = BTreeSet::new();
    let mut out = vec![];
    while !remaining.is_empty() {
        let (ready, rest): (Vec<&Step>, Vec<&Step>) = remaining
            .into_iter()
            .partition(|s| s.depends_on.iter().all(|d| done.contains(d.as_str())));
        if ready.is_empty() {
            return Err(Error::invalid("dependency cycle"));
        }
        done.extend(ready.iter().map(|s| s.id.as_str()));
        out.push(ready.iter().map(|s| s.id.clone()).collect());
        remaining = rest;
    }
    Ok(out)
}

// ---- planners ------------------------------------------------------------------------------

/// What a planner is given.
#[derive(Debug, Clone)]
pub struct GoalInput {
    pub title: String,
    pub text: String,
    pub repo: String,
    /// Harness ids that can take work.
    pub harnesses: Vec<String>,
    pub max_steps: u32,
}

pub trait Planner: Send + Sync {
    fn name(&self) -> &str;
    fn plan(&self, g: &GoalInput) -> Result<Plan>;
}

pub struct HeuristicPlanner;

fn classify(text: &str) -> StepKind {
    let t = text.to_ascii_lowercase();
    let has = |ws: &[&str]| ws.iter().any(|w| t.contains(w));
    if has(&["refactor", "restructure", "clean up", "rename"]) {
        StepKind::Refactor
    } else if has(&["write tests", "add tests", "test ", "tests", "spec "]) && !has(&["implement"])
    {
        StepKind::Test
    } else if has(&["document", "readme", "docs", "changelog"]) {
        StepKind::Docs
    } else if has(&[
        "investigate",
        "research",
        "explore",
        "figure out",
        "find out",
        "survey",
    ]) {
        StepKind::Research
    } else if has(&["review", "audit", "check that"]) {
        StepKind::Review
    } else {
        StepKind::Implement
    }
}

fn effort_of(text: &str) -> Effort {
    let t = text.to_ascii_lowercase();
    if ["quick", "small", "tiny", "trivial", "typo"]
        .iter()
        .any(|w| t.contains(w))
    {
        Effort::Quick
    } else if [
        "migrate", "rewrite", "refactor", "redesign", "overhaul", "large",
    ]
    .iter()
    .any(|w| t.contains(w))
        || t.len() > 400
    {
        Effort::Deep
    } else {
        Effort::Minutes
    }
}

/// Repository-relative path globs mentioned in backticks: `src/auth/` becomes `src/auth/**`.
fn paths_in(text: &str) -> Vec<String> {
    let mut out = vec![];
    for (i, part) in text.split('`').enumerate() {
        if i % 2 == 0 || part.is_empty() || part.contains(' ') {
            continue;
        }
        let p = part.trim_start_matches("./");
        if !(p.contains('/') || p.contains('*')) || p.starts_with('/') || p.contains("..") {
            continue;
        }
        let g = if p.ends_with('/') {
            format!("{p}**")
        } else {
            p.to_string()
        };
        if !out.contains(&g) {
            out.push(g);
        }
    }
    out
}

fn short_title(s: &str) -> String {
    let t = s.trim().trim_end_matches(['.', ':']);
    let first = t.lines().next().unwrap_or("");
    let mut o: String = first.chars().take(72).collect();
    if first.chars().count() > 72 {
        o.push('…');
    }
    o
}

#[derive(Debug)]
struct Item {
    text: String,
    numbered: bool,
}

fn list_items(text: &str) -> Vec<Item> {
    let num = regex::Regex::new(r"^\s*\d+[.)]\s+(.*)$").unwrap();
    let bullet = regex::Regex::new(r"^\s*[-*+]\s+(?:\[( |x|X)\]\s+)?(.*)$").unwrap();
    let mut items: Vec<Item> = vec![];
    for line in text.lines() {
        if let Some(c) = num.captures(line) {
            items.push(Item {
                text: c[1].trim().to_string(),
                numbered: true,
            });
        } else if let Some(c) = bullet.captures(line) {
            if matches!(c.get(1).map(|m| m.as_str()), Some("x") | Some("X")) {
                // already done
                items.push(Item {
                    text: String::new(),
                    numbered: false,
                });
            } else {
                items.push(Item {
                    text: c[2].trim().to_string(),
                    numbered: false,
                });
            }
        } else if !line.trim().is_empty()
            && line.starts_with(char::is_whitespace)
            && let Some(last) = items.last_mut()
            && !last.text.is_empty()
        {
            last.text.push('\n');
            last.text.push_str(line.trim());
        }
    }
    items.retain(|i| !i.text.is_empty());
    items
}

fn heading_items(text: &str) -> Vec<Item> {
    let mut items: Vec<Item> = vec![];
    for line in text.lines() {
        if let Some(h) = line
            .strip_prefix("## ")
            .or_else(|| line.strip_prefix("### "))
        {
            items.push(Item {
                text: h.trim().to_string(),
                numbered: false,
            });
        } else if let Some(last) = items.last_mut()
            && !line.trim().is_empty()
            && !line.starts_with('#')
        {
            last.text.push('\n');
            last.text.push_str(line.trim());
        }
    }
    items
}

impl Planner for HeuristicPlanner {
    fn name(&self) -> &str {
        "heuristic"
    }

    fn plan(&self, g: &GoalInput) -> Result<Plan> {
        let body = if g.text.trim().is_empty() {
            g.title.as_str()
        } else {
            g.text.as_str()
        };
        let mut items = list_items(body);
        if items.is_empty() {
            items = heading_items(body);
        }
        let mut steps: Vec<Step> = vec![];
        if items.is_empty() {
            steps.push(Step {
                id: "s1".into(),
                title: short_title(&g.title),
                prompt: body.trim().to_string(),
                depends_on: vec![],
                kind: classify(body),
                effort: effort_of(body),
                harness: None,
                priority: 0,
                paths: paths_in(body),
            });
        } else {
            let sequential = items.iter().all(|i| i.numbered);
            for (n, it) in items.iter().enumerate() {
                let id = format!("s{}", n + 1);
                steps.push(Step {
                    title: short_title(&it.text),
                    prompt: format!("{}\n\nPart of the goal: {}", it.text.trim(), g.title.trim()),
                    depends_on: if sequential && n > 0 {
                        vec![format!("s{n}")]
                    } else {
                        vec![]
                    },
                    kind: classify(&it.text),
                    effort: effort_of(&it.text),
                    harness: None,
                    priority: 0,
                    paths: paths_in(&it.text),
                    id,
                });
            }
        }
        let plan = Plan {
            rationale: format!(
                "{} step(s) read from the structure of the goal text ({}).",
                steps.len(),
                if steps.len() == 1 {
                    "no list found, one step"
                } else if steps.iter().any(|s| !s.depends_on.is_empty()) {
                    "numbered list, run in order"
                } else {
                    "independent items, run in parallel"
                }
            ),
            planner: self.name().into(),
            created_at_ms: now_ms(),
            steps,
        };
        let problems = validate(&plan, g.max_steps);
        if problems.is_empty() {
            Ok(plan)
        } else {
            Err(Error::invalid(problems.join("; ")))
        }
    }
}

/// A planner that returns a fixed result (tests, replays).
pub struct ScriptedPlanner {
    pub result: std::sync::Mutex<Option<Result<Plan>>>,
}
impl ScriptedPlanner {
    pub fn new(r: Result<Plan>) -> Self {
        ScriptedPlanner {
            result: std::sync::Mutex::new(Some(r)),
        }
    }
}
impl Planner for ScriptedPlanner {
    fn name(&self) -> &str {
        "scripted"
    }
    fn plan(&self, _g: &GoalInput) -> Result<Plan> {
        self.result
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| Err(Error::invalid("scripted planner has no more plans")))
    }
}

/// The instruction given to a planning agent (`backend = "agent"`). It must end by calling
/// `submit_cmd`, which validates and stores the plan; the agent does not start any work.
pub fn planner_prompt(g: &GoalInput, submit_cmd: &str) -> String {
    format!(
        "You are planning, not implementing. Do not edit any file in the repository.\n\n\
GOAL: {title}\n\n{text}\n\n\
Break the goal into at most {max} steps that can each be given to one coding agent in its own git worktree. \
Prefer independent steps; add `depends_on` only where one step needs another's result. \
Write the plan as JSON of this shape and nothing else in the file:\n\n\
{{\"rationale\": \"why this split\", \"steps\": [{{\"id\": \"s1\", \"title\": \"short title\", \"prompt\": \"complete instructions for the agent\", \
\"depends_on\": [], \"kind\": \"implement|test|docs|research|review|refactor|other\", \"effort\": \"quick|minutes|deep\", \
\"harness\": null, \"paths\": [\"src/area/**\"]}}]}}\n\n\
Available harnesses: {harnesses}. `harness` may name one of them or be null.\n\
Save the JSON to a file and run: {submit}\n",
        title = g.title.trim(),
        text = g.text.trim(),
        max = g.max_steps,
        harnesses = if g.harnesses.is_empty() {
            "any".to_string()
        } else {
            g.harnesses.join(", ")
        },
        submit = submit_cmd
    )
}

/// Parse a plan from JSON text, tolerating a surrounding Markdown fence or prose. The result
/// is validated.
pub fn parse_plan_json(text: &str, max_steps: u32, planner: &str) -> Result<Plan> {
    let t = text.trim();
    let candidate = if let Some(start) = t.find("```") {
        let rest = &t[start + 3..];
        let rest = rest.strip_prefix("json").unwrap_or(rest);
        rest.split("```").next().unwrap_or(rest).trim().to_string()
    } else if let (Some(a), Some(b)) = (t.find('{'), t.rfind('}')) {
        t[a..=b].to_string()
    } else {
        t.to_string()
    };
    let v: Value = serde_json::from_str(&candidate)
        .map_err(|e| Error::invalid(format!("the plan is not valid JSON: {e}")))?;
    let mut plan: Plan = serde_json::from_value(v)
        .map_err(|e| Error::invalid(format!("the plan does not have the expected shape: {e}")))?;
    plan.planner = planner.to_string();
    plan.created_at_ms = now_ms();
    let problems = validate(&plan, max_steps);
    if problems.is_empty() {
        Ok(plan)
    } else {
        Err(Error::invalid(problems.join("; ")))
    }
}

// ---- goals and the approval gate -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalState {
    Draft,
    Planned,
    Approved,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl GoalState {
    pub fn as_str(self) -> &'static str {
        match self {
            GoalState::Draft => "draft",
            GoalState::Planned => "planned",
            GoalState::Approved => "approved",
            GoalState::Running => "running",
            GoalState::Done => "done",
            GoalState::Failed => "failed",
            GoalState::Cancelled => "cancelled",
        }
    }
    pub fn closed(self) -> bool {
        matches!(
            self,
            GoalState::Done | GoalState::Failed | GoalState::Cancelled
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    #[default]
    Pending,
    Running,
    Done,
    Failed,
    /// A dependency failed; this step will not run.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct StepRun {
    pub status: StepStatus,
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub started_at_ms: Option<i64>,
    #[serde(default)]
    pub finished_at_ms: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub handle: String,
    pub title: String,
    pub text: String,
    pub repo: String,
    #[serde(default)]
    pub base: Option<String>,
    pub created_at_ms: i64,
    pub state: GoalState,
    #[serde(default)]
    pub plan: Option<Plan>,
    #[serde(default)]
    pub plan_rev: u32,
    #[serde(default)]
    pub approved_rev: Option<u32>,
    #[serde(default)]
    pub approved_by: Option<String>,
    #[serde(default)]
    pub approved_at_ms: Option<i64>,
    #[serde(default)]
    pub runs: BTreeMap<String, StepRun>,
    /// Id of a planning-agent run, while the `agent` backend is working.
    #[serde(default)]
    pub planning_run: Option<String>,
}

impl Goal {
    pub fn new(
        id: &str,
        handle: &str,
        title: &str,
        text: &str,
        repo: &str,
        base: Option<&str>,
    ) -> Goal {
        Goal {
            id: id.into(),
            handle: handle.into(),
            title: title.into(),
            text: text.into(),
            repo: repo.into(),
            base: base.map(str::to_string),
            created_at_ms: now_ms(),
            state: GoalState::Draft,
            plan: None,
            plan_rev: 0,
            approved_rev: None,
            approved_by: None,
            approved_at_ms: None,
            runs: BTreeMap::new(),
            planning_run: None,
        }
    }

    /// Store a plan (draft or planned goals only). Any earlier approval is cleared.
    pub fn set_plan(&mut self, plan: Plan, max_steps: u32) -> Result<()> {
        if !matches!(
            self.state,
            GoalState::Draft | GoalState::Planned | GoalState::Approved
        ) {
            return Err(Error::Refused(format!(
                "a {} goal cannot take a new plan",
                self.state.as_str()
            )));
        }
        let problems = validate(&plan, max_steps);
        if !problems.is_empty() {
            return Err(Error::invalid(problems.join("; ")));
        }
        if self.state == GoalState::Approved
            && self.runs.values().any(|r| r.status != StepStatus::Pending)
        {
            return Err(Error::Refused(
                "work has started; cancel the goal to replan".into(),
            ));
        }
        self.plan = Some(plan);
        self.plan_rev += 1;
        self.approved_rev = None;
        self.approved_by = None;
        self.approved_at_ms = None;
        self.planning_run = None;
        self.state = GoalState::Planned;
        Ok(())
    }

    /// Approve the current revision of the plan.
    pub fn approve(&mut self, by: &str, max_steps: u32) -> Result<()> {
        if self.state != GoalState::Planned {
            return Err(Error::Refused(format!(
                "only a planned goal can be approved (this one is {})",
                self.state.as_str()
            )));
        }
        let plan = self
            .plan
            .as_ref()
            .ok_or_else(|| Error::Refused("no plan to approve".into()))?;
        let problems = validate(plan, max_steps);
        if !problems.is_empty() {
            return Err(Error::invalid(problems.join("; ")));
        }
        self.approved_rev = Some(self.plan_rev);
        self.approved_by = Some(by.to_string());
        self.approved_at_ms = Some(now_ms());
        self.state = GoalState::Approved;
        Ok(())
    }

    /// May steps start? True once approved at the current revision, or for a planned goal when
    /// the config does not require approval.
    pub fn may_start(&self, approval_required: bool) -> bool {
        match self.state {
            GoalState::Approved | GoalState::Running => self.approved_rev == Some(self.plan_rev),
            GoalState::Planned => !approval_required,
            _ => false,
        }
    }

    pub fn status_of(&self, step: &str) -> StepStatus {
        self.runs.get(step).map(|r| r.status).unwrap_or_default()
    }

    /// Pending steps whose dependencies are all done, in plan order.
    pub fn ready_steps(&self) -> Vec<&Step> {
        let Some(p) = &self.plan else { return vec![] };
        p.steps
            .iter()
            .filter(|s| self.status_of(&s.id) == StepStatus::Pending)
            .filter(|s| {
                s.depends_on
                    .iter()
                    .all(|d| self.status_of(d) == StepStatus::Done)
            })
            .collect()
    }

    pub fn step_started(&mut self, step: &str, task: &str, harness: &str) -> Result<()> {
        let known = self
            .plan
            .as_ref()
            .is_some_and(|p| p.steps.iter().any(|s| s.id == step));
        if !known {
            return Err(Error::invalid(format!("no step {step}")));
        }
        if self.status_of(step) != StepStatus::Pending {
            return Err(Error::Refused(format!("step {step} already started")));
        }
        self.runs.insert(
            step.to_string(),
            StepRun {
                status: StepStatus::Running,
                task: Some(task.to_string()),
                harness: Some(harness.to_string()),
                started_at_ms: Some(now_ms()),
                ..Default::default()
            },
        );
        if self.state == GoalState::Approved || self.state == GoalState::Planned {
            self.state = GoalState::Running;
        }
        Ok(())
    }

    /// Record a step's end. A failure skips the steps that depend on it (directly or not).
    pub fn step_finished(&mut self, step: &str, ok: bool, error: Option<&str>) -> Result<()> {
        let Some(r) = self.runs.get_mut(step) else {
            return Err(Error::invalid(format!("step {step} has not started")));
        };
        if r.status != StepStatus::Running {
            return Err(Error::Refused(format!("step {step} is not running")));
        }
        r.status = if ok {
            StepStatus::Done
        } else {
            StepStatus::Failed
        };
        r.finished_at_ms = Some(now_ms());
        r.error = error.map(str::to_string);
        if !ok {
            self.skip_dependents(step);
        }
        self.settle();
        Ok(())
    }

    fn skip_dependents(&mut self, failed: &str) {
        let Some(p) = &self.plan else { return };
        let mut bad: BTreeSet<String> = BTreeSet::from([failed.to_string()]);
        loop {
            let more: Vec<String> = p
                .steps
                .iter()
                .filter(|s| !bad.contains(&s.id) && s.depends_on.iter().any(|d| bad.contains(d)))
                .map(|s| s.id.clone())
                .collect();
            if more.is_empty() {
                break;
            }
            bad.extend(more);
        }
        bad.remove(failed);
        for id in bad {
            let e = self.runs.entry(id).or_default();
            if e.status == StepStatus::Pending {
                e.status = StepStatus::Skipped;
                e.error = Some(format!("{failed} failed"));
            }
        }
    }

    /// Move the goal to done or failed when nothing is left to run.
    fn settle(&mut self) {
        let Some(p) = &self.plan else { return };
        let st: Vec<StepStatus> = p.steps.iter().map(|s| self.status_of(&s.id)).collect();
        if st
            .iter()
            .any(|s| matches!(s, StepStatus::Pending | StepStatus::Running))
        {
            return;
        }
        self.state = if st.iter().all(|s| *s == StepStatus::Done) {
            GoalState::Done
        } else {
            GoalState::Failed
        };
    }

    pub fn cancel(&mut self) -> Result<()> {
        if self.state.closed() {
            return Err(Error::Refused(format!(
                "the goal is already {}",
                self.state.as_str()
            )));
        }
        if let Some(p) = &self.plan {
            for s in &p.steps {
                self.runs.entry(s.id.clone()).or_default();
            }
            for r in self.runs.values_mut() {
                if r.status == StepStatus::Pending {
                    r.status = StepStatus::Skipped;
                    r.error = Some("goal cancelled".into());
                }
            }
        }
        self.state = GoalState::Cancelled;
        Ok(())
    }

    /// (done, total) over the plan's steps.
    pub fn progress(&self) -> (usize, usize) {
        match &self.plan {
            Some(p) => (
                p.steps
                    .iter()
                    .filter(|s| self.status_of(&s.id) == StepStatus::Done)
                    .count(),
                p.steps.len(),
            ),
            None => (0, 0),
        }
    }
}

// ---- routing --------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct HarnessProfile {
    pub id: String,
    pub kinds: Vec<StepKind>,
    /// 1 cheap, 2 middle, 3 expensive.
    pub cost_tier: u8,
    pub available: bool,
    pub max_parallel: u32,
}

/// Built-in rough strengths for the harnesses Vibeke knows; users override with config later.
pub fn default_profile(id: &str) -> HarnessProfile {
    use StepKind::*;
    let (kinds, tier): (&[StepKind], u8) = match id {
        "claude" => (
            &[Implement, Refactor, Test, Docs, Research, Review, Other],
            3,
        ),
        "codex" => (&[Implement, Refactor, Test, Review], 2),
        "pi" | "omp" => (&[Research, Docs, Implement, Other], 1),
        "opencode" | "gemini" | "hermes" => (&[Implement, Docs, Research, Test], 2),
        _ => (&[Implement, Other], 2),
    };
    HarnessProfile {
        id: id.to_string(),
        kinds: kinds.to_vec(),
        cost_tier: tier,
        available: true,
        max_parallel: 4,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountState {
    pub harness: String,
    pub used_fraction: Option<f64>,
    pub limited: bool,
    pub resets_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub harness: String,
    pub score: f64,
    pub reasons: Vec<String>,
}

/// Pick the harness for a step: fit to the step's kind, cost against effort, quota headroom,
/// and current load. A harness held by a rate limit or at its parallel cap is excluded.
pub fn route(
    step: &Step,
    profiles: &[HarnessProfile],
    accounts: &[AccountState],
    running: &BTreeMap<String, u32>,
    now_ms: i64,
) -> Result<Route> {
    let mut best: Option<Route> = None;
    let mut excluded: Vec<String> = vec![];
    for p in profiles {
        let acct = accounts.iter().find(|a| a.harness == p.id);
        if !p.available {
            excluded.push(format!("{} is not available", p.id));
            continue;
        }
        if let Some(a) = acct
            && a.limited
            && a.resets_at_ms.is_none_or(|t| t > now_ms)
        {
            excluded.push(format!(
                "{} is rate limited{}",
                p.id,
                a.resets_at_ms
                    .map(|t| format!(" for another {} min", ((t - now_ms) / 60_000).max(1)))
                    .unwrap_or_default()
            ));
            continue;
        }
        let load = running.get(&p.id).copied().unwrap_or(0);
        if load >= p.max_parallel {
            excluded.push(format!(
                "{} is at its limit of {} parallel run(s)",
                p.id, p.max_parallel
            ));
            continue;
        }
        let mut score = 0.0;
        let mut reasons = vec![];
        if step.harness.as_deref() == Some(p.id.as_str()) {
            score += 50.0;
            reasons.push("the plan asks for it".to_string());
        }
        if p.kinds.contains(&step.kind) {
            score += 20.0;
            reasons.push(format!("good at {}", step.kind.as_str()));
        }
        match (step.effort, p.cost_tier) {
            (Effort::Deep, 3) => {
                score += 10.0;
                reasons.push("deep work suits the strongest tier".into());
            }
            (Effort::Quick, 1) => {
                score += 10.0;
                reasons.push("quick work suits the cheap tier".into());
            }
            (Effort::Quick, 3) => score -= 5.0,
            _ => {}
        }
        if let Some(u) = acct.and_then(|a| a.used_fraction) {
            score += 30.0 * (1.0 - u.clamp(0.0, 1.0));
            reasons.push(format!(
                "{:.0}% of its quota used",
                u.clamp(0.0, 1.0) * 100.0
            ));
        }
        score -= 10.0 * f64::from(load);
        if load > 0 {
            reasons.push(format!("{load} run(s) already going"));
        }
        if best.as_ref().is_none_or(|b| score > b.score) {
            best = Some(Route {
                harness: p.id.clone(),
                score,
                reasons,
            });
        }
    }
    best.ok_or_else(|| {
        Error::Refused(if excluded.is_empty() {
            "no harness is configured to take work".to_string()
        } else {
            format!(
                "no harness can take step {} now: {}",
                step.id,
                excluded.join("; ")
            )
        })
    })
}

// ---- briefings ------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct BriefEvent {
    pub ts_ms: i64,
    pub kind: String,
    pub subject: Value,
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BriefSection {
    pub title: String,
    pub items: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Briefing {
    pub since_ms: i64,
    pub until_ms: i64,
    pub sections: Vec<BriefSection>,
    pub counts: BTreeMap<String, u32>,
    pub text: String,
}

fn s_of(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

/// A deterministic digest of what happened between `since_ms` and `until_ms`. `names` maps
/// task ids to handles/titles so lines read as people talk about them.
pub fn briefing(
    events: &[BriefEvent],
    since_ms: i64,
    until_ms: i64,
    names: &BTreeMap<String, String>,
) -> Briefing {
    let name = |id: &str| names.get(id).cloned().unwrap_or_else(|| id.to_string());
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut goals = vec![];
    let mut tasks = vec![];
    let mut attention = vec![];
    let mut merges = vec![];
    let mut limits = vec![];
    for e in events
        .iter()
        .filter(|e| e.ts_ms >= since_ms && e.ts_ms <= until_ms)
    {
        *counts.entry(e.kind.clone()).or_default() += 1;
        match e.kind.as_str() {
            "goal.created" => goals.push(format!("goal {} created", s_of(&e.data, "title"))),
            "goal.planned" => goals.push(format!(
                "goal {} planned in {} step(s)",
                s_of(&e.subject, "goal"),
                e.data.get("steps").and_then(Value::as_u64).unwrap_or(0)
            )),
            "goal.approved" => goals.push(format!(
                "goal {} approved by {}",
                s_of(&e.subject, "goal"),
                s_of(&e.data, "by")
            )),
            "goal.step_started" => goals.push(format!(
                "goal {} step {} started on {}",
                s_of(&e.subject, "goal"),
                s_of(&e.subject, "step"),
                s_of(&e.data, "harness")
            )),
            "goal.step_finished" => goals.push(format!(
                "goal {} step {} {}",
                s_of(&e.subject, "goal"),
                s_of(&e.subject, "step"),
                s_of(&e.data, "status")
            )),
            "goal.finished" => goals.push(format!(
                "goal {} {}",
                s_of(&e.subject, "goal"),
                s_of(&e.data, "state")
            )),
            "task.created" => tasks.push(format!("started {}", s_of(&e.data, "title"))),
            "task.finished" => tasks.push(format!(
                "{} {}",
                name(&s_of(&e.subject, "task")),
                s_of(&e.data, "status")
            )),
            "task.setup_failed" => tasks.push(format!(
                "setup failed in {}",
                name(&s_of(&e.subject, "task"))
            )),
            "task.parked" => tasks.push(format!("{} parked", name(&s_of(&e.subject, "task")))),
            "task.resumed" => tasks.push(format!("{} resumed", name(&s_of(&e.subject, "task")))),
            "interaction.opened" => {
                attention.push(format!("{} asked for a decision", s_of(&e.subject, "run")))
            }
            "interaction.decided" => attention.push(format!(
                "{} {} by {}",
                s_of(&e.subject, "interaction"),
                e.data
                    .get("decision")
                    .and_then(Value::as_str)
                    .unwrap_or("answered"),
                s_of(&e.data, "by")
            )),
            "merge.merged" => merges.push(format!(
                "{} merged into {}",
                s_of(&e.data, "handle"),
                s_of(&e.data, "target")
            )),
            "merge.conflict" => merges.push(format!(
                "{} conflicts with {}",
                s_of(&e.data, "handle"),
                s_of(&e.data, "target")
            )),
            "merge.check_failed" => {
                merges.push(format!("{} failed its check", s_of(&e.data, "handle")))
            }
            "merge.conflict_predicted" => merges.push(format!(
                "predicted {} conflict between {} and {}",
                s_of(&e.data, "severity"),
                s_of(&e.data, "a"),
                s_of(&e.data, "b")
            )),
            "agent.rate_limited" => {
                limits.push(format!("{} hit a rate limit", s_of(&e.subject, "run")))
            }
            "quota.paused" => limits.push(format!(
                "paused {} until the limit resets",
                s_of(&e.data, "handle")
            )),
            "quota.resumed" => limits.push(format!("resumed {}", s_of(&e.data, "handle"))),
            _ => {}
        }
    }
    let mut sections = vec![];
    for (title, items) in [
        ("Goals", goals),
        ("Tasks", tasks),
        ("Needs you", attention),
        ("Merges", merges),
        ("Limits", limits),
    ] {
        if !items.is_empty() {
            sections.push(BriefSection {
                title: title.into(),
                items,
            });
        }
    }
    let mut text = String::new();
    if sections.is_empty() {
        text.push_str("Nothing happened in this window.\n");
    }
    for s in &sections {
        text.push_str(&format!("{}\n", s.title));
        for i in &s.items {
            text.push_str(&format!("  - {i}\n"));
        }
    }
    Briefing {
        since_ms,
        until_ms,
        sections,
        counts,
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(id: &str, deps: &[&str]) -> Step {
        Step {
            id: id.into(),
            title: format!("title {id}"),
            prompt: format!("do {id}"),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            kind: StepKind::Implement,
            effort: Effort::Minutes,
            harness: None,
            priority: 0,
            paths: vec![],
        }
    }

    fn plan(steps: Vec<Step>) -> Plan {
        Plan {
            steps,
            rationale: "r".into(),
            planner: "t".into(),
            created_at_ms: 0,
        }
    }

    fn input(text: &str) -> GoalInput {
        GoalInput {
            title: "Ship the thing".into(),
            text: text.into(),
            repo: "/r".into(),
            harnesses: vec!["claude".into(), "codex".into()],
            max_steps: 12,
        }
    }

    #[test]
    fn validation_catches_every_structural_problem() {
        assert!(validate(&plan(vec![step("a", &[]), step("b", &["a"])]), 12).is_empty());
        assert!(!validate(&plan(vec![]), 12).is_empty());
        let e = validate(&plan(vec![step("a", &[]), step("a", &[])]), 12);
        assert!(e.iter().any(|x| x.contains("duplicate")));
        let e = validate(&plan(vec![step("a", &["zzz"])]), 12);
        assert!(e.iter().any(|x| x.contains("unknown step")));
        let e = validate(&plan(vec![step("a", &["a"])]), 12);
        assert!(e.iter().any(|x| x.contains("itself")));
        let e = validate(&plan(vec![step("a", &["b"]), step("b", &["a"])]), 12);
        assert!(e.iter().any(|x| x.contains("cycle")), "{e:?}");
        let e = validate(
            &plan(vec![step("a", &[]), step("b", &[]), step("c", &[])]),
            2,
        );
        assert!(e.iter().any(|x| x.contains("limit")));
        let mut bad = step("a b", &[]);
        bad.title = " ".into();
        bad.prompt = "".into();
        bad.paths = vec!["/abs".into()];
        let e = validate(&plan(vec![bad]), 12);
        assert!(e.len() >= 4, "{e:?}");
    }

    #[test]
    fn levels_group_by_depth() {
        let p = plan(vec![
            step("a", &[]),
            step("b", &[]),
            step("c", &["a", "b"]),
            step("d", &["c"]),
        ]);
        assert_eq!(
            levels(&p).unwrap(),
            vec![vec!["a", "b"], vec!["c"], vec!["d"]]
        );
        assert!(levels(&plan(vec![step("a", &["b"]), step("b", &["a"])])).is_err());
    }

    #[test]
    fn heuristic_reads_numbered_lists_as_sequential() {
        let g = input(
            "Plan:\n1. Add the schema migration in `db/migrations/`\n2. Write tests for the new endpoint\n   including the 404 case\n3) Document it in the README",
        );
        let p = HeuristicPlanner.plan(&g).unwrap();
        assert_eq!(p.steps.len(), 3);
        assert_eq!(p.steps[0].depends_on, Vec::<String>::new());
        assert_eq!(p.steps[1].depends_on, vec!["s1"]);
        assert_eq!(p.steps[2].depends_on, vec!["s2"]);
        assert_eq!(p.steps[0].paths, vec!["db/migrations/**"]);
        assert_eq!(p.steps[1].kind, StepKind::Test);
        assert!(p.steps[1].prompt.contains("404 case"));
        assert_eq!(p.steps[2].kind, StepKind::Docs);
        assert_eq!(p.planner, "heuristic");
        assert!(p.rationale.contains("in order"));
        assert!(validate(&p, 12).is_empty());
    }

    #[test]
    fn heuristic_reads_bullets_as_parallel_and_skips_checked_items() {
        let g = input(
            "- [ ] speed up the parser\n- [x] already done thing\n- investigate flaky login test\n- refactor `src/net/`",
        );
        let p = HeuristicPlanner.plan(&g).unwrap();
        assert_eq!(p.steps.len(), 3);
        assert!(p.steps.iter().all(|s| s.depends_on.is_empty()));
        assert_eq!(p.steps[1].kind, StepKind::Research);
        assert_eq!(p.steps[2].kind, StepKind::Refactor);
        assert_eq!(p.steps[2].effort, Effort::Deep);
        assert_eq!(p.steps[2].paths, vec!["src/net/**"]);
    }

    #[test]
    fn heuristic_falls_back_to_headings_then_one_step() {
        let p = HeuristicPlanner
            .plan(&input("## Backend\nadd the API\n## Frontend\nadd the form"))
            .unwrap();
        assert_eq!(p.steps.len(), 2);
        assert!(p.steps[0].prompt.contains("add the API"));
        let p = HeuristicPlanner
            .plan(&input("Just make login faster."))
            .unwrap();
        assert_eq!(p.steps.len(), 1);
        let mut g = input("");
        g.title = "Quick typo fix in the readme".into();
        let p = HeuristicPlanner.plan(&g).unwrap();
        assert_eq!(p.steps[0].effort, Effort::Quick);
        // too many items fail validation
        let many: String = (1..=20).map(|i| format!("- item {i}\n")).collect();
        assert!(HeuristicPlanner.plan(&input(&many)).is_err());
    }

    #[test]
    fn scripted_planner_returns_what_it_was_given_once() {
        let sp = ScriptedPlanner::new(Ok(plan(vec![step("a", &[])])));
        assert_eq!(sp.plan(&input("x")).unwrap().steps.len(), 1);
        assert!(sp.plan(&input("x")).is_err());
        let sp = ScriptedPlanner::new(Err(Error::invalid("nope")));
        assert!(sp.plan(&input("x")).is_err());
    }

    #[test]
    fn plan_json_is_extracted_and_validated() {
        let ok = r#"Here you go:
```json
{"rationale": "split", "steps": [
  {"id": "s1", "title": "A", "prompt": "do A", "kind": "implement", "effort": "quick", "paths": ["src/a/**"]},
  {"id": "s2", "title": "B", "prompt": "do B", "depends_on": ["s1"], "kind": "test"}]}
```"#;
        let p = parse_plan_json(ok, 12, "agent:claude").unwrap();
        assert_eq!(p.steps.len(), 2);
        assert_eq!(p.planner, "agent:claude");
        assert_eq!(p.steps[1].depends_on, vec!["s1"]);
        assert_eq!(p.steps[0].effort, Effort::Quick);
        let bare = r#"{"steps": [{"id": "x", "title": "t", "prompt": "p"}]}"#;
        assert!(parse_plan_json(bare, 12, "x").is_ok());
        assert!(parse_plan_json("no json here", 12, "x").is_err());
        assert!(parse_plan_json(r#"{"steps": [{"id": "x"}]}"#, 12, "x").is_err());
        assert!(parse_plan_json(r#"{"steps": []}"#, 12, "x").is_err());
        assert!(
            parse_plan_json(
                r#"{"steps": [{"id": "a", "title": "t", "prompt": "p", "depends_on": ["a"]}]}"#,
                12,
                "x"
            )
            .is_err()
        );
    }

    #[test]
    fn prompt_names_the_submit_command_and_forbids_edits() {
        let p = planner_prompt(
            &input("goal text"),
            "vibeke goal plan-submit g1 --file plan.json",
        );
        assert!(p.contains("Do not edit any file"));
        assert!(p.contains("vibeke goal plan-submit g1 --file plan.json"));
        assert!(p.contains("at most 12 steps"));
        assert!(p.contains("claude, codex"));
    }

    fn goal() -> Goal {
        Goal::new("id1", "G1", "Ship", "text", "/r", Some("main"))
    }

    #[test]
    fn approval_gate_blocks_work_until_the_current_revision_is_approved() {
        let mut g = goal();
        assert!(!g.may_start(true));
        assert!(g.approve("me", 12).is_err(), "no plan yet");
        g.set_plan(plan(vec![step("a", &[]), step("b", &["a"])]), 12)
            .unwrap();
        assert_eq!(g.state, GoalState::Planned);
        assert!(!g.may_start(true), "planned but not approved");
        assert!(g.may_start(false), "approval not required");
        g.approve("me", 12).unwrap();
        assert!(g.may_start(true));
        assert_eq!(g.approved_by.as_deref(), Some("me"));
        // Editing the plan clears the approval.
        g.set_plan(
            plan(vec![step("a", &[]), step("b", &["a"]), step("c", &[])]),
            12,
        )
        .unwrap();
        assert_eq!(g.state, GoalState::Planned);
        assert!(!g.may_start(true));
        assert_eq!(g.approved_rev, None);
        assert_eq!(g.plan_rev, 2);
        // invalid plans are refused and change nothing
        assert!(g.set_plan(plan(vec![]), 12).is_err());
        assert_eq!(g.plan_rev, 2);
        g.approve("me", 12).unwrap();
        assert!(g.approve("me", 12).is_err(), "already approved");
    }

    #[test]
    fn fan_out_follows_dependencies_and_failures_skip_dependents() {
        let mut g = goal();
        g.set_plan(
            plan(vec![
                step("a", &[]),
                step("b", &[]),
                step("c", &["a"]),
                step("d", &["c"]),
                step("e", &["b"]),
            ]),
            12,
        )
        .unwrap();
        g.approve("me", 12).unwrap();
        let ready: Vec<_> = g.ready_steps().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ready, vec!["a", "b"]);
        g.step_started("a", "t1", "claude").unwrap();
        assert_eq!(g.state, GoalState::Running);
        assert!(g.step_started("a", "t9", "claude").is_err());
        assert!(g.step_started("zzz", "t9", "claude").is_err());
        g.step_started("b", "t2", "codex").unwrap();
        assert!(g.ready_steps().is_empty());
        g.step_finished("a", false, Some("boom")).unwrap();
        assert_eq!(g.status_of("c"), StepStatus::Skipped);
        assert_eq!(g.status_of("d"), StepStatus::Skipped);
        assert_eq!(g.status_of("e"), StepStatus::Pending);
        assert_eq!(g.state, GoalState::Running, "b is still running");
        g.step_finished("b", true, None).unwrap();
        assert_eq!(g.ready_steps().len(), 1);
        g.step_started("e", "t3", "claude").unwrap();
        g.step_finished("e", true, None).unwrap();
        assert_eq!(g.state, GoalState::Failed);
        assert_eq!(g.progress(), (2, 5));
        assert!(g.step_finished("e", true, None).is_err());
    }

    #[test]
    fn all_steps_done_finishes_the_goal_and_cancel_closes_it() {
        let mut g = goal();
        g.set_plan(plan(vec![step("a", &[]), step("b", &["a"])]), 12)
            .unwrap();
        g.approve("me", 12).unwrap();
        g.step_started("a", "t1", "claude").unwrap();
        g.step_finished("a", true, None).unwrap();
        g.step_started("b", "t2", "claude").unwrap();
        g.step_finished("b", true, None).unwrap();
        assert_eq!(g.state, GoalState::Done);
        assert!(g.cancel().is_err());
        let mut g2 = goal();
        g2.set_plan(plan(vec![step("a", &[]), step("b", &["a"])]), 12)
            .unwrap();
        g2.approve("me", 12).unwrap();
        g2.step_started("a", "t1", "claude").unwrap();
        g2.cancel().unwrap();
        assert_eq!(g2.state, GoalState::Cancelled);
        assert_eq!(g2.status_of("b"), StepStatus::Skipped);
        assert!(g2.set_plan(plan(vec![step("z", &[])]), 12).is_err());
        let j = serde_json::to_string(&g2).unwrap();
        assert_eq!(serde_json::from_str::<Goal>(&j).unwrap(), g2);
    }

    #[test]
    fn replanning_after_work_started_is_refused() {
        let mut g = goal();
        g.set_plan(plan(vec![step("a", &[])]), 12).unwrap();
        g.approve("me", 12).unwrap();
        g.step_started("a", "t1", "claude").unwrap();
        assert!(
            g.set_plan(plan(vec![step("a", &[]), step("b", &[])]), 12)
                .is_err()
        );
    }

    fn profiles() -> Vec<HarnessProfile> {
        vec![
            default_profile("claude"),
            default_profile("codex"),
            default_profile("pi"),
        ]
    }

    #[test]
    fn routing_weighs_fit_cost_and_quota() {
        let mut s = step("a", &[]);
        let none: BTreeMap<String, u32> = BTreeMap::new();
        // Deep refactor: claude (strongest, fits).
        s.kind = StepKind::Refactor;
        s.effort = Effort::Deep;
        let r = route(&s, &profiles(), &[], &none, 0).unwrap();
        assert_eq!(r.harness, "claude");
        // Quick docs: cheap tier wins.
        s.kind = StepKind::Docs;
        s.effort = Effort::Quick;
        assert_eq!(route(&s, &profiles(), &[], &none, 0).unwrap().harness, "pi");
        // The plan's hint wins when the harness has headroom...
        s.harness = Some("codex".into());
        assert_eq!(
            route(&s, &profiles(), &[], &none, 0).unwrap().harness,
            "codex"
        );
        // ...but not when it is rate limited.
        let acct = [AccountState {
            harness: "codex".into(),
            used_fraction: Some(1.0),
            limited: true,
            resets_at_ms: Some(10 * 60_000),
        }];
        assert_ne!(
            route(&s, &profiles(), &acct, &none, 0).unwrap().harness,
            "codex"
        );
        // After the reset it is back.
        assert_eq!(
            route(&s, &profiles(), &acct, &none, 11 * 60_000)
                .unwrap()
                .harness,
            "codex"
        );
        // Headroom breaks a tie between equals.
        s.harness = None;
        s.kind = StepKind::Implement;
        s.effort = Effort::Minutes;
        let acct = [
            AccountState {
                harness: "claude".into(),
                used_fraction: Some(0.95),
                limited: false,
                resets_at_ms: None,
            },
            AccountState {
                harness: "codex".into(),
                used_fraction: Some(0.1),
                limited: false,
                resets_at_ms: None,
            },
            AccountState {
                harness: "pi".into(),
                used_fraction: Some(0.95),
                limited: false,
                resets_at_ms: None,
            },
        ];
        assert_eq!(
            route(&s, &profiles(), &acct, &none, 0).unwrap().harness,
            "codex"
        );
        // Load pushes work elsewhere and the cap excludes.
        let mut load = BTreeMap::new();
        load.insert("codex".to_string(), 4u32);
        assert_ne!(
            route(&s, &profiles(), &acct, &load, 0).unwrap().harness,
            "codex"
        );
        // Everything limited: an error that says why.
        let all: Vec<AccountState> = ["claude", "codex", "pi"]
            .iter()
            .map(|h| AccountState {
                harness: h.to_string(),
                used_fraction: Some(1.0),
                limited: true,
                resets_at_ms: None,
            })
            .collect();
        let e = route(&s, &profiles(), &all, &none, 0)
            .unwrap_err()
            .to_string();
        assert!(e.contains("rate limited"), "{e}");
        assert!(route(&s, &[], &[], &none, 0).is_err());
        let mut off = profiles();
        for p in &mut off {
            p.available = false;
        }
        assert!(
            route(&s, &off, &[], &none, 0)
                .unwrap_err()
                .to_string()
                .contains("not available")
        );
    }

    #[test]
    fn briefing_digests_the_window() {
        let ev = |ts: i64, kind: &str, subject: Value, data: Value| BriefEvent {
            ts_ms: ts,
            kind: kind.into(),
            subject,
            data,
        };
        let events = vec![
            ev(
                5,
                "task.finished",
                json!({"task": "t0"}),
                json!({"status": "finished"}),
            ),
            ev(
                100,
                "goal.created",
                json!({"goal": "G1"}),
                json!({"title": "Ship it"}),
            ),
            ev(
                110,
                "goal.planned",
                json!({"goal": "G1"}),
                json!({"steps": 3}),
            ),
            ev(
                120,
                "goal.step_finished",
                json!({"goal": "G1", "step": "s1"}),
                json!({"status": "done"}),
            ),
            ev(
                130,
                "task.finished",
                json!({"task": "t1"}),
                json!({"status": "finished"}),
            ),
            ev(
                140,
                "interaction.decided",
                json!({"interaction": "i4"}),
                json!({"decision": "allow", "by": "policy"}),
            ),
            ev(
                150,
                "merge.merged",
                json!({}),
                json!({"handle": "k7.2", "target": "main"}),
            ),
            ev(160, "agent.rate_limited", json!({"run": "a3"}), json!({})),
            ev(170, "quota.paused", json!({}), json!({"handle": "k9"})),
            ev(
                900,
                "task.finished",
                json!({"task": "t2"}),
                json!({"status": "late"}),
            ),
        ];
        let mut names = BTreeMap::new();
        names.insert("t1".to_string(), "k5".to_string());
        let b = briefing(&events, 50, 500, &names);
        let titles: Vec<_> = b.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["Goals", "Tasks", "Needs you", "Merges", "Limits"]
        );
        assert!(b.text.contains("goal G1 planned in 3 step(s)"));
        assert!(b.text.contains("k5 finished"), "{}", b.text);
        assert!(b.text.contains("k7.2 merged into main"));
        assert!(!b.text.contains("late"));
        assert_eq!(b.counts["task.finished"], 1);
        let empty = briefing(&[], 0, 10, &names);
        assert!(empty.text.contains("Nothing happened"));
        assert!(empty.sections.is_empty());
    }
}
