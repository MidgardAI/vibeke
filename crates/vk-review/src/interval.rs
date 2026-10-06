//! Execution-interval binding (spec 15 §6.3, lane 3F).
//!
//! A command an agent ran is evidence for a subject only when the collector can show the whole
//! subject was **stable for the whole execution interval**. The rules, from the spec:
//!
//! - Matching start/end hashes alone cannot exclude intermediate changes (write, test, revert).
//!   So the collector also keeps a [`WriteJournal`]: every non-ignored write under the checkout
//!   from a watcher that was armed before the command started and never lost an event (no
//!   overflow, no eviction) until after it ended.
//! - The command is `Bound` only when the journal covers the interval completely, holds no
//!   relevant write inside it, no other known writer was active, and the checkout state captured
//!   at the start equals the one captured at the end.
//! - Anything else is `Unbound` with the reasons; the UI then offers a stable commit or an
//!   isolated snapshot ([`OFFER`]).
//! - A binding names a checkout *state* ([`CodeState`]); [`IntervalBinding::subject_for`] turns it
//!   into a subject id only for a subject that state shows exactly (a committed subject = clean
//!   tree at its head; a dirty snapshot = same head and change digest).
//!
//! Pure: capturing the states and feeding the journal is the server's job.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::screenshot::CodeState;
use crate::subject::{ChangeSubject, DirtyState};

/// What the UI offers when evidence cannot be bound.
pub const OFFER: &str = "Select a stable commit or take an isolated snapshot to bind evidence";

/// Most events a journal keeps; older ones are evicted and shrink the covered window.
pub const DEFAULT_MAX_EVENTS: usize = 20_000;

/// One write observed under a checkout (path relative to the checkout root).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteEvent {
    pub at_ms: i64,
    pub path: String,
}

/// Why a window is not fully covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Complete,
    /// No watcher was armed for this checkout.
    NotArmed,
    /// The watcher started after the interval began.
    ArmedAfterStart,
    /// The watcher lost events (overflow, restart, error) inside or just before the window.
    Gap,
    /// The journal evicted events that belong to the window.
    Evicted,
}

/// Write events of one checkout plus the facts needed to say whether they are *complete* for a
/// time window.
#[derive(Debug, Clone)]
pub struct WriteJournal {
    armed_at_ms: Option<i64>,
    /// Time of the newest event or heartbeat; an overflow gap reaches back to it.
    last_activity_ms: Option<i64>,
    /// `[from, to]` spans during which events may have been lost.
    gaps: Vec<(i64, i64)>,
    /// A watcher outage still open since this time.
    open_gap_from: Option<i64>,
    events: VecDeque<WriteEvent>,
    max_events: usize,
    /// Events at or before this time were evicted.
    evicted_through_ms: Option<i64>,
}

impl Default for WriteJournal {
    fn default() -> Self {
        WriteJournal::new(DEFAULT_MAX_EVENTS)
    }
}

impl WriteJournal {
    pub fn new(max_events: usize) -> Self {
        WriteJournal {
            armed_at_ms: None,
            last_activity_ms: None,
            gaps: Vec::new(),
            open_gap_from: None,
            events: VecDeque::new(),
            max_events: max_events.max(1),
            evicted_through_ms: None,
        }
    }

    /// The watcher is up and delivering from `at_ms`.
    pub fn arm(&mut self, at_ms: i64) {
        self.armed_at_ms.get_or_insert(at_ms);
        self.last_activity_ms.get_or_insert(at_ms);
        if let Some(from) = self.open_gap_from.take() {
            self.gaps.push((from, at_ms));
        }
    }

    /// The watcher went away (error, restart, stop): nothing is known from `at_ms` until the
    /// next [`arm`](Self::arm).
    pub fn disarm(&mut self, at_ms: i64) {
        self.open_gap_from.get_or_insert(at_ms);
    }

    pub fn is_armed(&self) -> bool {
        self.armed_at_ms.is_some() && self.open_gap_from.is_none()
    }

    /// The watcher reported that it may have dropped events (e.g. an inotify/FSEvents overflow
    /// or a "rescan" flag): everything since the last sign of life is unknown.
    pub fn mark_rescan(&mut self, at_ms: i64) {
        let from = self.last_activity_ms.or(self.armed_at_ms).unwrap_or(at_ms);
        self.gaps.push((from.min(at_ms), at_ms));
        self.last_activity_ms = Some(at_ms);
    }

    /// Proof of life with no write (a periodic probe): narrows what an overflow can hide.
    pub fn heartbeat(&mut self, at_ms: i64) {
        if self.last_activity_ms.is_none_or(|t| at_ms > t) {
            self.last_activity_ms = Some(at_ms);
        }
    }

    /// A write observed at `at_ms`.
    pub fn record(&mut self, path: impl Into<String>, at_ms: i64) {
        self.events.push_back(WriteEvent {
            at_ms,
            path: path.into(),
        });
        self.heartbeat(at_ms);
        while self.events.len() > self.max_events {
            if let Some(e) = self.events.pop_front() {
                self.evicted_through_ms =
                    Some(self.evicted_through_ms.map_or(e.at_ms, |t| t.max(e.at_ms)));
            }
        }
    }

    /// Drop events older than `before_ms`, remembering that they are gone.
    pub fn prune_before(&mut self, before_ms: i64) {
        while self.events.front().is_some_and(|e| e.at_ms < before_ms) {
            if let Some(e) = self.events.pop_front() {
                self.evicted_through_ms =
                    Some(self.evicted_through_ms.map_or(e.at_ms, |t| t.max(e.at_ms)));
            }
        }
        self.gaps.retain(|(_, to)| *to >= before_ms);
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn armed_at_ms(&self) -> Option<i64> {
        self.armed_at_ms
    }

    pub fn gap_count(&self) -> usize {
        self.gaps.len() + usize::from(self.open_gap_from.is_some())
    }

    /// Is the journal a complete record of writes in `[start_ms, end_ms]`?
    pub fn coverage(&self, start_ms: i64, end_ms: i64) -> Coverage {
        let Some(armed) = self.armed_at_ms else {
            return Coverage::NotArmed;
        };
        if armed > start_ms {
            return Coverage::ArmedAfterStart;
        }
        if self.open_gap_from.is_some_and(|from| from <= end_ms)
            || self
                .gaps
                .iter()
                .any(|(from, to)| *from <= end_ms && *to >= start_ms)
        {
            return Coverage::Gap;
        }
        if self.evicted_through_ms.is_some_and(|t| t >= start_ms) {
            return Coverage::Evicted;
        }
        Coverage::Complete
    }

    /// Events in `[start_ms, end_ms]` that are not ignored by `is_ignored`.
    pub fn writes_in(
        &self,
        start_ms: i64,
        end_ms: i64,
        is_ignored: &dyn Fn(&str) -> bool,
    ) -> Vec<WriteEvent> {
        self.events
            .iter()
            .filter(|e| e.at_ms >= start_ms && e.at_ms <= end_ms && !is_ignored(&e.path))
            .cloned()
            .collect()
    }

    /// Distinct paths written in `[start_ms, end_ms]` (for one ignore check by the caller).
    pub fn paths_in(&self, start_ms: i64, end_ms: i64) -> Vec<String> {
        let mut v: Vec<String> = self
            .events
            .iter()
            .filter(|e| e.at_ms >= start_ms && e.at_ms <= end_ms)
            .map(|e| e.path.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    }
}

/// Paths inside Git's own directory: HEAD, index and ref changes show up in the captured state
/// instead, and Git writes there constantly.
pub fn is_git_internal(rel: &str) -> bool {
    let rel = rel.trim_start_matches("./");
    rel == ".git" || rel.starts_with(".git/")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnboundReason {
    /// Start or end time of the command is missing, or the end precedes the start.
    MissingTimes,
    /// The checkout state could not be captured at the start of the interval.
    NoStartState {
        detail: String,
    },
    /// The checkout state could not be captured at the end of the interval.
    NoEndState {
        detail: String,
    },
    /// A state is `unknown` (capture incomplete), never treated as clean.
    StateUnknown,
    /// Start and end states differ: the command (or something else) changed the subject.
    SubjectChanged,
    WatcherNotArmed,
    /// The watcher started after the command did.
    WatcherLate,
    /// The watcher may have missed events (overflow, restart) in or before the interval.
    WatcherGap,
    /// The journal no longer holds the whole window.
    JournalEvicted,
    /// Non-ignored files were written during the interval (the first is named).
    WritesObserved {
        count: usize,
        first_path: String,
    },
    /// Another run or process was known to be writing in the same checkout.
    OtherWriter {
        who: String,
    },
}

impl UnboundReason {
    pub fn text(&self) -> String {
        match self {
            UnboundReason::MissingTimes => "the command's start or end time is unknown".into(),
            UnboundReason::NoStartState { detail } => {
                format!("the checkout could not be captured when the command started ({detail})")
            }
            UnboundReason::NoEndState { detail } => {
                format!("the checkout could not be captured when the command ended ({detail})")
            }
            UnboundReason::StateUnknown => "the checkout state could not be fully captured".into(),
            UnboundReason::SubjectChanged => {
                "the checkout changed between the command's start and end".into()
            }
            UnboundReason::WatcherNotArmed => {
                "no file watcher covered this checkout; writes could not be excluded".into()
            }
            UnboundReason::WatcherLate => "the file watcher started after the command did".into(),
            UnboundReason::WatcherGap => {
                "the file watcher may have missed events during the command".into()
            }
            UnboundReason::JournalEvicted => {
                "the write journal no longer holds the whole command window".into()
            }
            UnboundReason::WritesObserved { count, first_path } => format!(
                "{count} file write(s) happened during the command (first: {first_path}); a matching start and end state cannot exclude an intermediate change"
            ),
            UnboundReason::OtherWriter { who } => {
                format!("{who} was writing in the same checkout")
            }
        }
    }
}

/// Everything [`decide`] needs about one command execution.
#[derive(Debug, Clone)]
pub struct IntervalInput<'a> {
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    /// `Err(detail)` when capture failed.
    pub start_state: Result<&'a CodeState, String>,
    pub end_state: Result<&'a CodeState, String>,
    /// Events can arrive after the write they describe; the window extends this far past the
    /// command's end.
    pub settle_ms: i64,
    /// Other known writers (other runs, shell processes) active in the same checkout during the
    /// interval.
    pub other_writers: Vec<String>,
}

/// The decision for one execution interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntervalBinding {
    pub status: IntervalStatus,
    /// The checkout state the command ran against (the end state; start == end when bound).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<CodeState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<UnboundReason>,
    pub window_start_ms: i64,
    pub window_end_ms: i64,
    pub decided_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntervalStatus {
    Bound,
    Unbound,
}

impl IntervalBinding {
    pub fn is_bound(&self) -> bool {
        self.status == IntervalStatus::Bound
    }

    /// Plain-language reasons for the UI, with the offer appended when unbound.
    pub fn explanation(&self) -> Vec<String> {
        if self.is_bound() {
            return vec![format!(
                "Subject stable for the whole command ({})",
                self.state
                    .as_ref()
                    .map(CodeState::label)
                    .unwrap_or_default()
            )];
        }
        let mut v: Vec<String> = self.reasons.iter().map(UnboundReason::text).collect();
        v.push(OFFER.into());
        v
    }

    /// The id of `subject` when this binding is established and its state shows exactly the
    /// subject's content; `None` otherwise (the command stays "code binding unverified").
    pub fn subject_for(&self, subject: &ChangeSubject) -> Option<String> {
        let state = self.state.as_ref()?;
        (self.is_bound() && state.matches_subject(subject)).then(|| subject.id.clone())
    }
}

fn same_state(a: &CodeState, b: &CodeState) -> bool {
    a.repo == b.repo
        && a.head_sha == b.head_sha
        && a.dirty_state == b.dirty_state
        && a.dirty_digest == b.dirty_digest
}

/// Decide whether the subject was stable for the whole interval. `is_ignored` says which
/// relative paths are ignored by Git (build output, caches): writes to them do not change the
/// subject. [`is_git_internal`] paths are always ignored here (their effect is in the captured
/// states).
pub fn decide(
    input: &IntervalInput<'_>,
    journal: &WriteJournal,
    is_ignored: &dyn Fn(&str) -> bool,
    now_ms: i64,
) -> IntervalBinding {
    let mut reasons = Vec::new();
    let (start, end) = match (input.start_ms, input.end_ms) {
        (Some(s), Some(e)) if e >= s => (s, e),
        _ => {
            reasons.push(UnboundReason::MissingTimes);
            (0, 0)
        }
    };
    let window_end = end.saturating_add(input.settle_ms.max(0));

    let start_state = match &input.start_state {
        Ok(s) => Some(*s),
        Err(d) => {
            reasons.push(UnboundReason::NoStartState { detail: d.clone() });
            None
        }
    };
    let end_state = match &input.end_state {
        Ok(s) => Some(*s),
        Err(d) => {
            reasons.push(UnboundReason::NoEndState { detail: d.clone() });
            None
        }
    };
    if let (Some(a), Some(b)) = (start_state, end_state) {
        if a.dirty_state == DirtyState::Unknown || b.dirty_state == DirtyState::Unknown {
            reasons.push(UnboundReason::StateUnknown);
        } else if !same_state(a, b) {
            reasons.push(UnboundReason::SubjectChanged);
        }
    }

    if !reasons.contains(&UnboundReason::MissingTimes) {
        match journal.coverage(start, window_end) {
            Coverage::Complete => {
                let writes =
                    journal.writes_in(start, window_end, &|p| is_git_internal(p) || is_ignored(p));
                if let Some(first) = writes.first() {
                    reasons.push(UnboundReason::WritesObserved {
                        count: writes.len(),
                        first_path: first.path.clone(),
                    });
                }
            }
            Coverage::NotArmed => reasons.push(UnboundReason::WatcherNotArmed),
            Coverage::ArmedAfterStart => reasons.push(UnboundReason::WatcherLate),
            Coverage::Gap => reasons.push(UnboundReason::WatcherGap),
            Coverage::Evicted => reasons.push(UnboundReason::JournalEvicted),
        }
    }
    if let Some(who) = input.other_writers.first() {
        reasons.push(UnboundReason::OtherWriter { who: who.clone() });
    }

    let bound = reasons.is_empty();
    IntervalBinding {
        status: if bound {
            IntervalStatus::Bound
        } else {
            IntervalStatus::Unbound
        },
        state: end_state.cloned(),
        reasons,
        window_start_ms: start,
        window_end_ms: window_end,
        decided_at_ms: now_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subject::{RepoIdentity, SubjectKind};

    fn state(head: &str, digest: Option<&str>) -> CodeState {
        CodeState {
            repo: "/r".into(),
            origin_url: None,
            head_sha: Some(head.into()),
            dirty_digest: digest.map(str::to_string),
            dirty_state: if digest.is_some() {
                DirtyState::Dirty
            } else {
                DirtyState::Clean
            },
            captured_at_ms: 0,
            warnings: vec![],
        }
    }

    fn armed(at: i64) -> WriteJournal {
        let mut j = WriteJournal::new(100);
        j.arm(at);
        j
    }

    fn input<'a>(s: &'a CodeState, e: &'a CodeState) -> IntervalInput<'a> {
        IntervalInput {
            start_ms: Some(1000),
            end_ms: Some(2000),
            start_state: Ok(s),
            end_state: Ok(e),
            settle_ms: 100,
            other_writers: vec![],
        }
    }

    fn none(_: &str) -> bool {
        false
    }

    #[test]
    fn quiet_covered_interval_with_equal_states_is_bound() {
        let s = state("aaa", None);
        let j = armed(500);
        let b = decide(&input(&s, &s.clone()), &j, &none, 3000);
        assert!(b.is_bound(), "{:?}", b.reasons);
        assert_eq!(b.state.as_ref().unwrap().head_sha.as_deref(), Some("aaa"));
        assert_eq!(b.window_end_ms, 2100);
        assert!(b.explanation()[0].contains("stable"));
    }

    #[test]
    fn matching_hashes_do_not_hide_an_intermediate_write() {
        // Write, test, revert: start == end, but the journal saw the write.
        let s = state("aaa", Some("d1"));
        let mut j = armed(500);
        j.record("src/lib.rs", 1500);
        let b = decide(&input(&s, &s.clone()), &j, &none, 3000);
        assert!(!b.is_bound());
        assert!(matches!(
            b.reasons[0],
            UnboundReason::WritesObserved { count: 1, .. }
        ));
        assert!(b.explanation().last().unwrap().contains("stable commit"));
    }

    #[test]
    fn ignored_and_git_internal_writes_do_not_matter() {
        let s = state("aaa", None);
        let mut j = armed(500);
        j.record("target/debug/x", 1200);
        j.record(".git/index.lock", 1300);
        let ignored = |p: &str| p.starts_with("target/");
        assert!(decide(&input(&s, &s.clone()), &j, &ignored, 3000).is_bound());
        // The same write is relevant when it is not ignored.
        let unignored = decide(&input(&s, &s.clone()), &j, &none, 3000);
        assert!(matches!(
            unignored.reasons[0],
            UnboundReason::WritesObserved { count: 1, .. }
        ));
        j.record("notes.txt", 1400);
        assert!(!decide(&input(&s, &s.clone()), &j, &ignored, 3000).is_bound());
    }

    #[test]
    fn late_events_inside_the_settle_window_count() {
        let s = state("aaa", None);
        let mut j = armed(500);
        j.record("a.rs", 2050); // delivered 50 ms after the command ended
        assert!(!decide(&input(&s, &s.clone()), &j, &none, 3000).is_bound());
        j.prune_before(0);
        let mut j2 = armed(500);
        j2.record("a.rs", 2200); // after the settle window: not this command
        assert!(decide(&input(&s, &s.clone()), &j2, &none, 3000).is_bound());
    }

    #[test]
    fn changed_state_between_start_and_end_is_unbound() {
        let a = state("aaa", None);
        let b = state("aaa", Some("d1"));
        let j = armed(500);
        let d = decide(&input(&a, &b), &j, &none, 3000);
        assert!(d.reasons.contains(&UnboundReason::SubjectChanged));
        let c = state("bbb", None);
        assert!(
            decide(&input(&a, &c), &j, &none, 3000)
                .reasons
                .contains(&UnboundReason::SubjectChanged)
        );
    }

    #[test]
    fn unknown_or_missing_state_is_unbound() {
        let a = state("aaa", None);
        let mut u = state("aaa", None);
        u.dirty_state = DirtyState::Unknown;
        let j = armed(500);
        assert!(
            decide(&input(&a, &u), &j, &none, 3000)
                .reasons
                .contains(&UnboundReason::StateUnknown)
        );
        let mut i = input(&a, &a);
        i.end_state = Err("git failed".into());
        let d = decide(&i, &j, &none, 3000);
        assert!(matches!(d.reasons[0], UnboundReason::NoEndState { .. }));
        assert!(d.state.is_none());
    }

    #[test]
    fn watcher_must_cover_the_whole_interval_without_gaps() {
        let s = state("aaa", None);
        let un = WriteJournal::new(10);
        assert_eq!(
            decide(&input(&s, &s.clone()), &un, &none, 1).reasons,
            vec![UnboundReason::WatcherNotArmed]
        );
        let late = armed(1500);
        assert_eq!(
            decide(&input(&s, &s.clone()), &late, &none, 1).reasons,
            vec![UnboundReason::WatcherLate]
        );
        // An overflow delivered after the command still hides events from inside it.
        let mut gap = armed(500);
        gap.heartbeat(900);
        gap.mark_rescan(2500);
        assert_eq!(
            decide(&input(&s, &s.clone()), &gap, &none, 1).reasons,
            vec![UnboundReason::WatcherGap]
        );
        // A rescan long before the command is fine.
        let mut old = armed(100);
        old.mark_rescan(300);
        old.heartbeat(900);
        assert!(decide(&input(&s, &s.clone()), &old, &none, 1).is_bound());
        // An outage that is still open.
        let mut down = armed(500);
        down.disarm(1800);
        assert_eq!(down.coverage(1000, 2100), Coverage::Gap);
        assert!(!down.is_armed());
        down.arm(2600);
        assert!(down.is_armed());
        assert_eq!(down.coverage(1000, 2100), Coverage::Gap);
        assert_eq!(down.coverage(3000, 3100), Coverage::Complete);
    }

    #[test]
    fn eviction_shrinks_the_covered_window() {
        let s = state("aaa", None);
        let mut j = WriteJournal::new(2);
        j.arm(0);
        j.record("a", 100);
        j.record("b", 200);
        j.record("c", 300); // evicts "a" at 100
        assert_eq!(j.len(), 2);
        assert_eq!(j.coverage(50, 400), Coverage::Evicted);
        assert_eq!(j.coverage(150, 400), Coverage::Complete);
        let mut i = input(&s, &s);
        i.start_ms = Some(50);
        i.end_ms = Some(60);
        assert_eq!(
            decide(&i, &j, &none, 1).reasons,
            vec![UnboundReason::JournalEvicted]
        );
    }

    #[test]
    fn other_writers_and_missing_times_are_unbound() {
        let s = state("aaa", None);
        let j = armed(500);
        let mut i = input(&s, &s);
        i.other_writers = vec!["run r2".into()];
        assert!(matches!(
            decide(&i, &j, &none, 1).reasons[0],
            UnboundReason::OtherWriter { .. }
        ));
        let mut t = input(&s, &s);
        t.end_ms = None;
        assert!(
            decide(&t, &j, &none, 1)
                .reasons
                .contains(&UnboundReason::MissingTimes)
        );
        let mut rev = input(&s, &s);
        rev.start_ms = Some(5000);
        rev.end_ms = Some(1000);
        assert!(
            decide(&rev, &j, &none, 1)
                .reasons
                .contains(&UnboundReason::MissingTimes)
        );
    }

    #[test]
    fn a_binding_names_a_subject_only_when_the_state_shows_it() {
        let head = "a".repeat(40);
        let subj = |dirty: Option<&str>, kind, ds| {
            ChangeSubject::new(
                RepoIdentity {
                    root: "/r".into(),
                    origin_url: None,
                },
                "b".repeat(40),
                head.clone(),
                dirty.map(str::to_string),
                ds,
                kind,
                1,
            )
        };
        let clean = state(&head, None);
        let j = armed(0);
        let b = decide(&input(&clean, &clean), &j, &none, 1);
        let committed = subj(None, SubjectKind::Committed, DirtyState::Clean);
        assert_eq!(b.subject_for(&committed), Some(committed.id.clone()));
        let other_head = ChangeSubject::new(
            committed.repo.clone(),
            "b".repeat(40),
            "c".repeat(40),
            None,
            DirtyState::Clean,
            SubjectKind::Committed,
            1,
        );
        assert!(b.subject_for(&other_head).is_none());
        let dirty_subject = subj(Some("d1"), SubjectKind::CheckoutLive, DirtyState::Dirty);
        assert!(b.subject_for(&dirty_subject).is_none());
        // A dirty state matches a dirty subject with the same digest.
        let dirty = state(&head, Some("d1"));
        let bd = decide(&input(&dirty, &dirty), &j, &none, 1);
        assert_eq!(
            bd.subject_for(&dirty_subject),
            Some(dirty_subject.id.clone())
        );
        // An unbound binding never names a subject.
        let mut jg = armed(0);
        jg.mark_rescan(1500);
        let ub = decide(&input(&clean, &clean), &jg, &none, 1);
        assert!(ub.subject_for(&committed).is_none());
    }
}
