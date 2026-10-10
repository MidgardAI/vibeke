//! The collision rules (05 §10), pure: a rolling window of touches per repo root, claims,
//! attribution of file-system changes to runs, and the records collisions are kept in.
//!
//! **Advisory.** A touch is evidence that a run attempted or reported an edit (adapter) or that
//! a path changed while the run was working (watcher, `git status`); it is not proof of who owns
//! the content. Findings warn; nothing here blocks, reverts or reassigns a change.
//!
//! **Confidence.** A touch is *reported* (the run's own tool call, or its process seen writing
//! the file), *inferred* (a change while that run was the only one working) or *ambiguous* (a
//! change while a few runs were working). Only reported evidence on both sides is `high`; a
//! guess on either side is at most `medium` and never notifies. A guess that the run on the
//! other side could have made itself explains nothing and raises nothing.

use super::glob::glob_match;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// How serious a collision is (05 §10 rules). Ordered: `Low < Medium < High`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Same directory/module, or a guessed edit of a file another run read: shown in the
    /// collision view only.
    Low,
    /// One run modified a file another run has read recently, or a same-file or claim hit where
    /// one side is a guess.
    Medium,
    /// The same file written by two runs, or a write inside a foreign claim, all reported.
    High,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
        }
    }
    pub fn parse(s: &str) -> Option<Severity> {
        Some(match s {
            "low" => Severity::Low,
            "medium" => Severity::Medium,
            "high" => Severity::High,
            _ => return None,
        })
    }
}

/// Where a touch came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// An adapter `file_change` item or a Read/Edit tool report (authoritative for that run).
    Adapter,
    /// A file-system watcher event, attributed by the rules in [`attribute`].
    Watcher,
    /// The `git status` poll.
    Git,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Adapter => "adapter",
            Source::Watcher => "watcher",
            Source::Git => "git",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Write,
    Read,
}

/// One observed read or write of a repo-relative path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Touch {
    /// The run, when attribution is certain.
    pub run: Option<String>,
    /// The runs that could have done it, when attribution is ambiguous (two or more).
    #[serde(default)]
    pub candidates: Vec<String>,
    pub path: String,
    pub kind: Kind,
    pub at_ms: i64,
    pub source: Source,
    pub op: String,
    /// `run` was inferred (the only run working when the change was seen), not reported.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inferred: bool,
}

impl Touch {
    pub fn write(run: &str, path: &str, at_ms: i64, source: Source, op: &str) -> Touch {
        Touch {
            run: Some(run.into()),
            candidates: vec![],
            path: path.into(),
            kind: Kind::Write,
            at_ms,
            source,
            op: op.into(),
            inferred: false,
        }
    }
    /// A write by `run` inferred from a change seen while it was the only run working.
    pub fn inferred(run: &str, path: &str, at_ms: i64, source: Source, op: &str) -> Touch {
        Touch {
            inferred: true,
            ..Touch::write(run, path, at_ms, source, op)
        }
    }
    pub fn read(run: &str, path: &str, at_ms: i64) -> Touch {
        Touch {
            run: Some(run.into()),
            candidates: vec![],
            path: path.into(),
            kind: Kind::Read,
            at_ms,
            source: Source::Adapter,
            op: "read".into(),
            inferred: false,
        }
    }
    pub fn ambiguous(candidates: Vec<String>, path: &str, at_ms: i64, source: Source) -> Touch {
        Touch {
            run: None,
            candidates,
            path: path.into(),
            kind: Kind::Write,
            at_ms,
            source,
            op: "modify".into(),
            inferred: false,
        }
    }
    /// One run is named (reported or inferred).
    pub fn is_certain(&self) -> bool {
        self.run.is_some()
    }
    /// The run reported this itself (or was seen writing): not a guess.
    pub fn is_reported(&self) -> bool {
        self.run.is_some() && !self.inferred
    }
    /// Every run this touch may belong to.
    pub fn who(&self) -> Vec<&str> {
        match &self.run {
            Some(r) => vec![r.as_str()],
            None => self.candidates.iter().map(String::as_str).collect(),
        }
    }
}

/// An advisory claim: a run says it works inside a glob (05 §10, Phase 2 groundwork).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub id: String,
    pub run: String,
    /// The repo root the glob is relative to.
    pub root: String,
    pub glob: String,
    pub created_ms: i64,
    #[serde(default)]
    pub note: Option<String>,
    /// More runs the claim belongs to (a task's claim binds every run of the task): a write by
    /// any of them is the owner's.
    #[serde(default)]
    pub also: Vec<String>,
}

impl Claim {
    pub fn covers(&self, path: &str) -> bool {
        glob_match(&self.glob, path)
    }

    /// Every run the claim belongs to.
    pub fn owners(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.run.as_str()).chain(self.also.iter().map(String::as_str))
    }
}

/// Why a finding was raised.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reason {
    /// The same file written by two runs.
    SameFile,
    /// Two runs wrote different files of one directory/module.
    SameDir { dir: String },
    /// `editor` modified a file `reader` read recently.
    ReadThenEdited {
        editor: Option<String>,
        reader: String,
    },
    /// A write inside `owner`'s claim.
    Claim {
        claim: String,
        owner: String,
        glob: String,
    },
}

impl Reason {
    pub fn kind(&self) -> &'static str {
        match self {
            Reason::SameFile => "same_file",
            Reason::SameDir { .. } => "same_dir",
            Reason::ReadThenEdited { .. } => "read_then_edited",
            Reason::Claim { .. } => "claim",
        }
    }
}

/// Whether a hit is announced: reported `high`, or a reported edit of a file another run read
/// (`medium` for that rule means reported). Guesses are shown, never announced.
pub fn notable(severity: Severity, reason: &Reason) -> bool {
    severity == Severity::High
        || (severity == Severity::Medium && matches!(reason, Reason::ReadThenEdited { .. }))
}

/// One rule hit for one touch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub reason: Reason,
    pub paths: Vec<String>,
    /// The runs involved (sorted). For an ambiguous touch these include every candidate.
    pub runs: Vec<String>,
    /// Attribution of at least one involved touch was ambiguous: "possibly".
    pub ambiguous: bool,
    pub at_ms: i64,
}

impl Finding {
    /// Whether this hit is evidence worth a notification ([`notable`]).
    pub fn notable(&self) -> bool {
        notable(self.severity, &self.reason)
    }
}

/// Rule parameters (from `[collision]`).
#[derive(Clone, Debug)]
pub struct Rules {
    pub window_ms: i64,
    pub read_window_ms: i64,
    pub dir_depth: usize,
    pub max_touches: usize,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            window_ms: 30 * 60 * 1000,
            read_window_ms: 10 * 60 * 1000,
            dir_depth: 2,
            max_touches: 5000,
        }
    }
}

/// The directory/module key of a path: its first `depth` directory components (`None` for a file
/// at the repo root or when the rule is off).
pub fn dir_key(path: &str, depth: usize) -> Option<String> {
    if depth == 0 {
        return None;
    }
    let comps: Vec<&str> = path.split('/').collect();
    if comps.len() < 2 {
        return None;
    }
    let dirs = &comps[..comps.len() - 1];
    Some(dirs[..dirs.len().min(depth)].join("/"))
}

/// The rolling window of touches of one repo root.
#[derive(Default, Debug)]
pub struct Tracker {
    touches: VecDeque<Touch>,
}

impl Tracker {
    pub fn new() -> Tracker {
        Tracker::default()
    }

    pub fn len(&self) -> usize {
        self.touches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.touches.is_empty()
    }

    pub fn touches(&self) -> impl Iterator<Item = &Touch> {
        self.touches.iter()
    }

    /// Drop touches older than the longer of the two windows.
    pub fn prune(&mut self, rules: &Rules, now_ms: i64) {
        let keep = rules.window_ms.max(rules.read_window_ms);
        while self
            .touches
            .front()
            .is_some_and(|t| now_ms.saturating_sub(t.at_ms) > keep)
        {
            self.touches.pop_front();
        }
        while self.touches.len() > rules.max_touches {
            self.touches.pop_front();
        }
    }

    /// Forget every touch of `path` (the user chose "ignore for this path").
    pub fn forget_path(&mut self, path: &str) {
        self.touches.retain(|t| t.path != path);
    }

    /// Forget every touch of `run` (it ended and was released).
    pub fn forget_run(&mut self, run: &str) {
        self.touches.retain(|t| t.run.as_deref() != Some(run));
    }

    /// Runs that touched anything within the window.
    pub fn active_runs(&self, rules: &Rules, now_ms: i64) -> BTreeSet<String> {
        self.touches
            .iter()
            .filter(|t| now_ms.saturating_sub(t.at_ms) <= rules.window_ms)
            .flat_map(|t| t.who().into_iter().map(str::to_string))
            .collect()
    }

    /// The touches of a path inside the window, oldest first (the collision timeline).
    pub fn timeline(&self, rules: &Rules, now_ms: i64, path: &str) -> Vec<Touch> {
        self.touches
            .iter()
            .filter(|t| t.path == path && now_ms.saturating_sub(t.at_ms) <= rules.window_ms)
            .cloned()
            .collect()
    }

    /// Record a touch and return what the rules say about it.
    pub fn record(&mut self, rules: &Rules, claims: &[Claim], t: Touch) -> Vec<Finding> {
        self.prune(rules, t.at_ms);
        let mut findings = Vec::new();
        if t.kind == Kind::Write {
            findings = self.evaluate(rules, claims, &t);
        }
        self.touches.push_back(t);
        while self.touches.len() > rules.max_touches {
            self.touches.pop_front();
        }
        findings
    }

    fn evaluate(&self, rules: &Rules, claims: &[Claim], t: &Touch) -> Vec<Finding> {
        let mut out: Vec<Finding> = Vec::new();
        let mine: BTreeSet<&str> = t.who().into_iter().collect();
        if mine.is_empty() {
            return out;
        }
        let in_window = |o: &&Touch| t.at_ms.saturating_sub(o.at_ms) <= rules.window_ms;

        // Same file: two runs wrote it. Needs at least one certain side; `high` needs both
        // sides reported.
        let mut file_runs: BTreeSet<String> = BTreeSet::new();
        let mut file_strong = false;
        let mut file_named = false;
        for o in self
            .touches
            .iter()
            .filter(in_window)
            .filter(|o| o.kind == Kind::Write && o.path == t.path)
        {
            if !distinct_writers(t, o) {
                continue;
            }
            file_runs.extend(t.who().into_iter().map(str::to_string));
            file_runs.extend(o.who().into_iter().map(str::to_string));
            file_strong |= t.is_reported() && o.is_reported();
            file_named |= t.is_certain() && o.is_certain();
        }
        if file_runs.len() >= 2 {
            out.push(Finding {
                severity: if file_strong {
                    Severity::High
                } else {
                    Severity::Medium
                },
                reason: Reason::SameFile,
                paths: vec![t.path.clone()],
                runs: file_runs.into_iter().collect(),
                ambiguous: !file_named,
                at_ms: t.at_ms,
            });
        }

        // A write inside someone else's claim.
        for c in claims.iter().filter(|c| c.covers(&t.path)) {
            // The owner writing (or an ambiguous writer who might be the owner): not a
            // violation we can state.
            if c.owners().any(|o| mine.contains(o)) {
                continue;
            }
            let mut runs: BTreeSet<String> = mine.iter().map(|r| (*r).to_string()).collect();
            runs.extend(c.owners().map(str::to_string));
            out.push(Finding {
                severity: if t.is_reported() {
                    Severity::High
                } else {
                    Severity::Medium
                },
                reason: Reason::Claim {
                    claim: c.id.clone(),
                    owner: c.run.clone(),
                    glob: c.glob.clone(),
                },
                paths: vec![t.path.clone()],
                runs: runs.into_iter().collect(),
                ambiguous: !t.is_certain(),
                at_ms: t.at_ms,
            });
        }

        // Read then edited: another run read this file recently.
        let mut readers: BTreeSet<String> = BTreeSet::new();
        for o in self.touches.iter().filter(|o| {
            o.kind == Kind::Read
                && o.path == t.path
                && t.at_ms.saturating_sub(o.at_ms) <= rules.read_window_ms
        }) {
            if let Some(r) = &o.run
                && !mine.contains(r.as_str())
            {
                readers.insert(r.clone());
            }
        }
        for reader in readers {
            let mut runs: BTreeSet<String> = mine.iter().map(|r| (*r).to_string()).collect();
            runs.insert(reader.clone());
            out.push(Finding {
                severity: if t.is_reported() {
                    Severity::Medium
                } else {
                    Severity::Low
                },
                reason: Reason::ReadThenEdited {
                    editor: t.run.clone(),
                    reader,
                },
                paths: vec![t.path.clone()],
                runs: runs.into_iter().collect(),
                ambiguous: !t.is_certain(),
                at_ms: t.at_ms,
            });
        }

        // Same directory/module: another run wrote a different file below the same key. Reported
        // writes only: a guess about a neighbouring file says nothing.
        if t.is_reported()
            && let Some(key) = dir_key(&t.path, rules.dir_depth)
        {
            let mut by_path: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            for o in self
                .touches
                .iter()
                .filter(in_window)
                .filter(|o| o.kind == Kind::Write && o.path != t.path && o.is_reported())
                .filter(|o| o.run != t.run)
                .filter(|o| dir_key(&o.path, rules.dir_depth).as_deref() == Some(key.as_str()))
            {
                by_path
                    .entry(o.path.clone())
                    .or_default()
                    .extend(o.run.clone());
            }
            if !by_path.is_empty() {
                let mut runs: BTreeSet<String> = mine.iter().map(|r| (*r).to_string()).collect();
                let mut paths = vec![t.path.clone()];
                for (p, rs) in by_path {
                    paths.push(p);
                    runs.extend(rs);
                }
                out.push(Finding {
                    severity: Severity::Low,
                    reason: Reason::SameDir { dir: key },
                    paths,
                    runs: runs.into_iter().collect(),
                    ambiguous: false,
                    at_ms: t.at_ms,
                });
            }
        }
        out
    }
}

/// Whether two writes of one path are evidence of two different writers. Two guesses never are;
/// a guess whose candidates include the named writer on the other side is most likely that
/// writer again (an agent formatting or rebuilding what it just edited).
fn distinct_writers(t: &Touch, o: &Touch) -> bool {
    match (&t.run, &o.run) {
        (Some(a), Some(b)) => a != b,
        (Some(a), None) => !o.candidates.contains(a),
        (None, Some(b)) => !t.candidates.contains(b),
        (None, None) => false,
    }
}

// ---- attribution ----------------------------------------------------------------------------

/// What the tracker knows about a run in the directory a change happened in.
#[derive(Clone, Debug, Default)]
pub struct RunView {
    pub run: String,
    /// In the `working` state right now.
    pub working: bool,
    /// Paths of tool calls the run reported as started and not yet reported finished (or only
    /// just finished), repo relative.
    pub in_flight: Vec<String>,
}

/// Most runs a change seen by the watcher or `git status` is guessed between. With more runs
/// working in one checkout, "one of them" says nothing and the change is dropped.
pub const MAX_CANDIDATES: usize = 3;

/// The result of attributing a file-system change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attribution {
    /// Reported: an in-flight tool call of the run, or its process seen writing the file.
    Run(String),
    /// The only run working when the change was seen.
    Inferred(String),
    /// One of a few runs working when the change was seen.
    Ambiguous(Vec<String>),
    /// No run was working there: a person, a formatter, a build: not an agent's collision.
    None,
}

fn pick(mut v: Vec<String>) -> Option<Attribution> {
    v.sort();
    v.dedup();
    match v.len() {
        0 => None,
        1 => Some(Attribution::Run(v.remove(0))),
        _ => Some(Attribution::Ambiguous(v)),
    }
}

/// Attribute a changed `path` (05 §10 signal 2): (a) a run that reported an in-flight tool call
/// touching that path; (b) the runs open-file sampling saw writing it (`fd_writers`, empty unless
/// `fs_attribution = "aggressive"` found any); (c) the runs working in that cwd at that moment:
/// inferred for one, ambiguous for up to [`MAX_CANDIDATES`], nothing for more.
pub fn attribute(path: &str, runs: &[RunView], fd_writers: &[String]) -> Attribution {
    let a: Vec<String> = runs
        .iter()
        .filter(|r| r.in_flight.iter().any(|p| p == path))
        .map(|r| r.run.clone())
        .collect();
    if let Some(r) = pick(a) {
        return r;
    }
    let known: BTreeSet<&str> = runs.iter().map(|r| r.run.as_str()).collect();
    let b: Vec<String> = fd_writers
        .iter()
        .filter(|w| known.contains(w.as_str()))
        .cloned()
        .collect();
    if let Some(r) = pick(b) {
        return r;
    }
    let c: Vec<String> = runs
        .iter()
        .filter(|r| r.working)
        .map(|r| r.run.clone())
        .collect();
    match pick(c) {
        Some(Attribution::Run(r)) => Attribution::Inferred(r),
        Some(Attribution::Ambiguous(v)) if v.len() > MAX_CANDIDATES => Attribution::None,
        Some(a) => a,
        None => Attribution::None,
    }
}

// ---- records --------------------------------------------------------------------------------

/// A collision record's lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Open,
    /// Every path went quiet for a window, fewer than two of its runs are still alive, or its
    /// checkout is no longer tracked.
    Cleared,
    /// The user ignored every path of it.
    Ignored,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Cleared => "cleared",
            Status::Ignored => "ignored",
        }
    }
}

/// One path of a collision record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathHit {
    pub path: String,
    pub severity: Severity,
    pub reason: Reason,
    pub runs: Vec<String>,
    pub ambiguous: bool,
    pub first_ms: i64,
    pub last_ms: i64,
}

/// A timeline entry (what the popup lists).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub at_ms: i64,
    pub run: Option<String>,
    #[serde(default)]
    pub candidates: Vec<String>,
    pub path: String,
    pub what: String,
    pub source: Source,
}

/// What the user sees: paths, runs, severity, timeline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollisionRec {
    pub id: String,
    pub root: String,
    pub severity: Severity,
    pub status: Status,
    pub runs: Vec<String>,
    pub paths: Vec<PathHit>,
    pub ambiguous: bool,
    pub first_ms: i64,
    pub last_ms: i64,
    #[serde(default)]
    pub timeline: Vec<TimelineEntry>,
    /// Path-set keys a notification was raised for.
    #[serde(default)]
    pub notified: Vec<(String, i64)>,
    #[serde(default)]
    pub cleared_ms: Option<i64>,
    #[serde(default)]
    pub cleared_reason: Option<String>,
}

/// What merging a finding changed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Merge {
    pub created: bool,
    pub new_paths: Vec<String>,
    pub new_runs: Vec<String>,
    pub severity_raised: bool,
}

impl Merge {
    /// Worth an event: a new record, a new path, a new run or a higher severity.
    pub fn changed(&self) -> bool {
        self.created
            || !self.new_paths.is_empty()
            || !self.new_runs.is_empty()
            || self.severity_raised
    }
}

/// Most timeline entries kept in a record.
pub const TIMELINE_MAX: usize = 200;
/// Most paths kept in a record: the weakest and oldest go first.
pub const PATHS_MAX: usize = 50;

impl CollisionRec {
    pub fn new(id: &str, root: &str, now_ms: i64) -> CollisionRec {
        CollisionRec {
            id: id.into(),
            root: root.into(),
            severity: Severity::Low,
            status: Status::Open,
            runs: vec![],
            paths: vec![],
            ambiguous: false,
            first_ms: now_ms,
            last_ms: now_ms,
            timeline: vec![],
            notified: vec![],
            cleared_ms: None,
            cleared_reason: None,
        }
    }

    /// Whether `f` belongs to this record: the open record of the same checkout. One record per
    /// checkout keeps one warning per place, whichever runs are involved.
    pub fn accepts(&self, root: &str, _f: &Finding) -> bool {
        self.status == Status::Open && self.root == root
    }

    /// Merge a finding. Severity only goes up while the record is open; paths are keyed by name
    /// and keep the strongest hit.
    pub fn merge(&mut self, f: &Finding) -> Merge {
        let mut m = Merge::default();
        let before = (!self.paths.is_empty()).then_some(self.severity);
        let runs_before = self.runs.clone();
        for p in &f.paths {
            match self.paths.iter_mut().find(|h| &h.path == p) {
                // A path names the runs and the confidence of its strongest evidence only: a
                // weaker hit (a guess, a same-directory hint) never adds its runs to it.
                Some(h) if f.severity > h.severity => {
                    h.last_ms = h.last_ms.max(f.at_ms);
                    h.severity = f.severity;
                    h.reason = f.reason.clone();
                    h.runs = f.runs.clone();
                    h.ambiguous = f.ambiguous;
                }
                Some(h) if f.severity == h.severity => {
                    h.last_ms = h.last_ms.max(f.at_ms);
                    for r in &f.runs {
                        if !h.runs.contains(r) {
                            h.runs.push(r.clone());
                        }
                    }
                    h.runs.sort();
                    h.ambiguous &= f.ambiguous;
                }
                Some(_) => {}
                None => {
                    self.paths.push(PathHit {
                        path: p.clone(),
                        severity: f.severity,
                        reason: f.reason.clone(),
                        runs: f.runs.clone(),
                        ambiguous: f.ambiguous,
                        first_ms: f.at_ms,
                        last_ms: f.at_ms,
                    });
                    m.new_paths.push(p.clone());
                }
            }
        }
        if self.paths.len() > PATHS_MAX {
            self.paths
                .sort_by(|a, b| b.severity.cmp(&a.severity).then(b.last_ms.cmp(&a.last_ms)));
            self.paths.truncate(PATHS_MAX);
            m.new_paths
                .retain(|p| self.paths.iter().any(|h| &h.path == p));
        }
        self.refresh();
        m.new_runs = self
            .runs
            .iter()
            .filter(|r| !runs_before.contains(r))
            .cloned()
            .collect();
        m.severity_raised = before.is_some_and(|b| self.severity > b);
        m.created = before.is_none();
        self.last_ms = self.last_ms.max(f.at_ms);
        m
    }

    /// Recompute severity, runs and the "possibly" flag from the paths.
    fn refresh(&mut self) {
        self.severity = self
            .paths
            .iter()
            .map(|h| h.severity)
            .max()
            .unwrap_or(Severity::Low);
        let runs: BTreeSet<String> = self
            .paths
            .iter()
            .flat_map(|h| h.runs.iter().cloned())
            .collect();
        self.runs = runs.into_iter().collect();
        self.ambiguous = self.paths.iter().all(|h| h.ambiguous);
    }

    /// Drop paths with no new hit for `window_ms` and the runs only they named. Returns whether
    /// anything was dropped; a record left with no path is quiet and should be cleared.
    pub fn expire(&mut self, now_ms: i64, window_ms: i64) -> bool {
        let n = self.paths.len();
        self.paths
            .retain(|h| now_ms.saturating_sub(h.last_ms) <= window_ms);
        if self.paths.len() == n {
            return false;
        }
        if !self.paths.is_empty() {
            self.refresh();
        }
        true
    }

    /// Whether any path is worth a notification: reported evidence of `high`, or a reported
    /// edit of a file another run read. Guesses are shown, never announced.
    pub fn notable(&self) -> bool {
        self.paths.iter().any(|h| notable(h.severity, &h.reason))
    }

    /// Append a timeline entry (bounded).
    pub fn note(&mut self, t: &Touch) {
        self.timeline.push(TimelineEntry {
            at_ms: t.at_ms,
            run: t.run.clone(),
            candidates: t.candidates.clone(),
            path: t.path.clone(),
            what: t.op.clone(),
            source: t.source,
        });
        if self.timeline.len() > TIMELINE_MAX {
            let drop = self.timeline.len() - TIMELINE_MAX;
            self.timeline.drain(..drop);
        }
    }

    /// The strongest paths first, as the sidebar line and notification name them.
    pub fn headline_paths(&self, max: usize) -> Vec<String> {
        let mut v: Vec<&PathHit> = self.paths.iter().collect();
        v.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.last_ms.cmp(&a.last_ms)));
        v.into_iter().take(max).map(|h| h.path.clone()).collect()
    }

    /// Drop a path (the user ignored it); the record is `ignored` when none is left.
    pub fn ignore_path(&mut self, path: &str) -> bool {
        let n = self.paths.len();
        self.paths.retain(|h| h.path != path);
        if self.paths.len() == n {
            return false;
        }
        if self.paths.is_empty() {
            self.status = Status::Ignored;
        } else {
            self.refresh();
        }
        true
    }
}

/// The key a notification is deduplicated on: the sorted path set (the severity class is part of
/// it, so a path set that escalates from medium to high is announced again).
pub fn path_set_key(paths: &[String], severity: Severity) -> String {
    let mut p: Vec<&str> = paths.iter().map(String::as_str).collect();
    p.sort_unstable();
    p.dedup();
    let mut h = blake3::Hasher::new();
    for x in p {
        h.update(x.as_bytes());
        h.update(&[0]);
    }
    h.update(severity.as_str().as_bytes());
    h.finalize().to_hex()[..16].to_string()
}

impl CollisionRec {
    /// Whether a notification for this path set was already raised inside `window_ms`; records
    /// the key when not.
    pub fn should_notify(&mut self, now_ms: i64, window_ms: i64) -> bool {
        // Keyed on the notable paths only: a guess joining the record changes nothing.
        let key = path_set_key(
            &self
                .paths
                .iter()
                .filter(|h| notable(h.severity, &h.reason))
                .map(|h| h.path.clone())
                .collect::<Vec<_>>(),
            self.severity,
        );
        self.notified
            .retain(|(_, at)| now_ms.saturating_sub(*at) <= window_ms);
        if self.notified.iter().any(|(k, _)| *k == key) {
            return false;
        }
        self.notified.push((key, now_ms));
        true
    }
}
