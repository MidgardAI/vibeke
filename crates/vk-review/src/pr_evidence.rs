//! Pull-request evidence (spec 15 §6.4, lane 3F).
//!
//! A PR observation records provider/repository/PR identity, target branch, head revision, draft
//! state, observation time and the authorization scope the lookup ran under. The rules:
//!
//! - A pasted URL or an agent statement is a [`PrClaim`]: it is shown, never confirmation, and
//!   never supports a criterion.
//! - An observation supports an external criterion only for the **committed subject whose head
//!   is exactly the observed PR head**, in the same repository. A changed PR head leaves the old
//!   observation bound to the old subject only; a newer observation is a new row.
//! - Failed or offline lookups and a PR nobody could find stay **unknown**; a stale observation
//!   (older than the configured age) is demoted to unknown.
//! - Merge and deployment observation are extension points only: a merged PR is recorded but
//!   never turned into a pass, and there is no release executor.
//!
//! This module is pure (no I/O). The lookup itself (the `gh` CLI, or a fake) lives in the
//! server.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::intent::{Evaluation, TaskIntent};
use crate::readiness::{Evidence, EvidenceCategory, EvidenceOutcome};
use crate::subject::{ChangeSubject, DirtyState, SubjectKind};
use crate::{Actor, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksRollup {
    None,
    Pending,
    Passing,
    Failing,
}

/// Provider, repository and number of a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrIdentity {
    /// `github` (any host reached through the `gh` CLI).
    pub provider: String,
    pub host: String,
    /// `owner/repo`.
    pub repository: String,
    pub number: u64,
    pub url: String,
}

impl PrIdentity {
    /// `host/owner/repo`, lower-case: comparable with [`normalize_remote`].
    pub fn canonical_repo(&self) -> String {
        format!("{}/{}", self.host, self.repository).to_ascii_lowercase()
    }
    pub fn key(&self) -> String {
        format!("{}#{}", self.canonical_repo(), self.number)
    }
}

/// What a successful lookup saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrFacts {
    pub identity: PrIdentity,
    pub target_branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_branch: Option<String>,
    /// The PR's head revision at observation time.
    pub head_sha: String,
    pub state: PrState,
    pub draft: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<String>,
    pub checks: ChecksRollup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Lookup {
    Observed {
        facts: PrFacts,
    },
    /// The provider answered: there is no pull request for that reference.
    NoPr,
    /// Offline, unauthenticated, timed out, unreadable answer ...: unknown, never pass or fail.
    Failed {
        reason: String,
    },
}

/// One observation row (immutable; a new lookup is a new row).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrObservation {
    pub id: String,
    pub task: String,
    /// What was asked for (PR number, URL or branch); `None` = the checked-out branch's PR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<String>,
    pub lookup: Lookup,
    pub observed_at_ms: i64,
    /// The credential scope the lookup ran under, e.g. `gh_cli` (the user's own `gh login`;
    /// read-only use) or `fake`.
    pub authorization_scope: String,
    /// Backend that answered: `gh` or `fake`.
    pub provider: String,
    pub observed_by: Actor,
    /// External criteria this observation is offered for (explicit user choice, default all).
    #[serde(default)]
    pub criterion_ids: Vec<String>,
}

impl PrObservation {
    pub fn facts(&self) -> Option<&PrFacts> {
        match &self.lookup {
            Lookup::Observed { facts } => Some(facts),
            _ => None,
        }
    }
    /// The PR key when known (from the facts).
    pub fn key(&self) -> Option<String> {
        self.facts().map(|f| f.identity.key())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimSource {
    PastedUrl,
    AgentStatement,
}

/// A PR someone *said* exists. Never confirmation (§6.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrClaim {
    pub id: String,
    pub task: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<PrIdentity>,
    pub source: ClaimSource,
    pub claimed_by: Actor,
    pub claimed_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl PrClaim {
    pub fn new(
        id: String,
        task: String,
        url: &str,
        source: ClaimSource,
        by: Actor,
        text: Option<String>,
    ) -> PrClaim {
        PrClaim {
            id,
            task,
            url: url.trim().to_string(),
            identity: parse_pr_url(url),
            source,
            claimed_by: by,
            claimed_at_ms: now_ms(),
            text,
        }
    }

    pub fn label(&self) -> String {
        match &self.identity {
            Some(i) => format!("Claimed {} (not confirmed)", i.key()),
            None => format!("Claimed pull request {} (not confirmed)", self.url),
        }
    }

    /// Agent-claim evidence: shown with its reason, never support.
    pub fn to_evidence(&self, external_criteria: &[String]) -> Evidence {
        Evidence {
            id: self.id.clone(),
            category: EvidenceCategory::AgentClaim,
            outcome: EvidenceOutcome::Unknown,
            check_definition_id: None,
            definition_digest: None,
            environment_digest: None,
            subject_id: None,
            criterion_ids: external_criteria.to_vec(),
            actor: Some(self.claimed_by.clone()),
            observed_at_ms: self.claimed_at_ms,
            summary: Some(self.label()),
        }
    }
}

/// The external criteria of an intent (the ones PR evidence can be offered for).
pub fn external_criteria(intent: Option<&TaskIntent>) -> Vec<String> {
    intent
        .map(|i| {
            i.criteria
                .iter()
                .filter(|c| c.evaluation == Evaluation::External)
                .map(|c| c.id.clone())
                .collect()
        })
        .unwrap_or_default()
}

// ---- parsing --------------------------------------------------------------------------------

/// `https://HOST/OWNER/REPO/pull/N[/...][?...][#...]`. Credentials in the authority, other
/// schemes and non-numeric numbers are refused.
pub fn parse_pr_url(url: &str) -> Option<PrIdentity> {
    let u = url.trim();
    let rest = u.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    if host.is_empty()
        || host.contains('@')
        || host.contains(char::is_whitespace)
        || host.starts_with('.')
    {
        return None;
    }
    let host = host.split(':').next()?.to_ascii_lowercase();
    let path = path.split(['?', '#']).next()?;
    let mut seg = path.split('/');
    let (owner, repo, kind, num) = (seg.next()?, seg.next()?, seg.next()?, seg.next()?);
    if kind != "pull" || owner.is_empty() || repo.is_empty() {
        return None;
    }
    let ok = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !ok(owner) || !ok(repo) {
        return None;
    }
    let number: u64 = num.parse().ok().filter(|n| *n > 0)?;
    Some(PrIdentity {
        provider: "github".into(),
        repository: format!("{owner}/{repo}"),
        number,
        url: format!("https://{host}/{owner}/{repo}/pull/{number}"),
        host,
    })
}

/// Normalize a git remote URL to `host/owner/repo` (lower-case, no `.git`, no credentials).
/// Handles `https://`, `ssh://`, `git://` and scp-like `git@host:owner/repo.git`.
pub fn normalize_remote(url: &str) -> Option<String> {
    let u = url.trim();
    let (host, path) = if let Some(rest) = ["https://", "http://", "ssh://", "git://"]
        .iter()
        .find_map(|p| u.strip_prefix(p))
    {
        let (authority, path) = rest.split_once('/')?;
        // Credentials end at the last `@` of the authority only.
        let host_port = authority.rsplit('@').next()?;
        (host_port.split(':').next()?, path)
    } else if !u.contains("://")
        && let Some((auth, path)) = u.split_once(':')
    {
        (auth.rsplit('@').next()?, path)
    } else {
        return None;
    };
    if host.is_empty() || host.contains(char::is_whitespace) {
        return None;
    }
    let path = path.split(['?', '#']).next()?.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut seg = path.split('/');
    let (owner, repo) = (seg.next()?, seg.next()?);
    if owner.is_empty() || repo.is_empty() || seg.next().is_some() {
        return None;
    }
    Some(format!("{host}/{owner}/{repo}").to_ascii_lowercase())
}

fn is_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn rollup(v: Option<&Value>) -> ChecksRollup {
    let Some(items) = v.and_then(Value::as_array).filter(|a| !a.is_empty()) else {
        return ChecksRollup::None;
    };
    let up = |it: &Value, k: &str| {
        it.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_uppercase()
    };
    let (mut failing, mut pending) = (false, false);
    for it in items {
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
        ChecksRollup::Failing
    } else if pending {
        ChecksRollup::Pending
    } else {
        ChecksRollup::Passing
    }
}

/// Parse `gh pr view --json number,url,state,isDraft,baseRefName,headRefName,headRefOid,
/// reviewDecision,statusCheckRollup`. Strict: a missing or inconsistent identity, head or target
/// makes the whole observation unusable (the caller records a failed lookup).
pub fn parse_gh_pr_view(json: &str) -> Result<PrFacts, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("unreadable gh output: {e}"))?;
    let text = |k: &str| v.get(k).and_then(Value::as_str);
    let url = text("url").ok_or("no url in gh output")?;
    let identity =
        parse_pr_url(url).ok_or_else(|| format!("unrecognized pull request url {url}"))?;
    let number = v
        .get("number")
        .and_then(Value::as_u64)
        .ok_or("no number in gh output")?;
    if number != identity.number {
        return Err("pull request number does not match its url".into());
    }
    let head_sha = text("headRefOid")
        .filter(|s| is_sha(s))
        .ok_or("no head revision in gh output")?
        .to_ascii_lowercase();
    let target = text("baseRefName")
        .filter(|s| !s.is_empty())
        .ok_or("no target branch in gh output")?;
    let state = match text("state").map(str::to_ascii_uppercase).as_deref() {
        Some("OPEN") => PrState::Open,
        Some("CLOSED") => PrState::Closed,
        Some("MERGED") => PrState::Merged,
        other => return Err(format!("unknown pull request state {other:?}")),
    };
    Ok(PrFacts {
        identity,
        target_branch: target.to_string(),
        head_branch: text("headRefName")
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        head_sha,
        state,
        draft: v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        review_decision: text("reviewDecision")
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        checks: rollup(v.get("statusCheckRollup")),
    })
}

// ---- assessment -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrUnbound {
    /// No reviewed subject to bind to.
    NoSubject,
    /// The PR head is not the subject's head (the PR moved, or the subject is older).
    HeadChanged,
    /// The subject is not a committed revision (dirty work is not in any PR).
    SubjectNotCommitted,
    RepositoryMismatch,
    /// The subject's repository has no recognizable origin to compare with.
    RepositoryUnverifiable,
    /// Merge observation is an extension point; judge manually.
    Merged,
    Draft,
    ChecksPending,
}

impl PrUnbound {
    pub fn text(self) -> &'static str {
        match self {
            PrUnbound::NoSubject => "no reviewed revision to compare with",
            PrUnbound::HeadChanged => "the pull request head is not this revision",
            PrUnbound::SubjectNotCommitted => "uncommitted changes are not in the pull request",
            PrUnbound::RepositoryMismatch => "the pull request is in a different repository",
            PrUnbound::RepositoryUnverifiable => {
                "the checkout's repository cannot be matched to the pull request"
            }
            PrUnbound::Merged => "merged; merge observation is not automated, judge manually",
            PrUnbound::Draft => "the pull request is a draft; judge manually",
            PrUnbound::ChecksPending => "its checks are still running; observe again later",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PrBinding {
    Bound,
    Unbound { reason: PrUnbound },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrAssessment {
    pub observation: String,
    pub binding: PrBinding,
    pub outcome: EvidenceOutcome,
    pub summary: String,
    /// Older than the allowed age: demoted to unknown.
    pub stale: bool,
}

impl PrAssessment {
    pub fn is_bound(&self) -> bool {
        self.binding == PrBinding::Bound
    }
}

fn unbound(obs: &PrObservation, reason: PrUnbound, what: &str) -> PrAssessment {
    PrAssessment {
        observation: obs.id.clone(),
        binding: PrBinding::Unbound { reason },
        outcome: EvidenceOutcome::Unknown,
        summary: format!("{what}: {}", reason.text()),
        stale: false,
    }
}

fn bound(obs: &PrObservation, outcome: EvidenceOutcome, summary: String) -> PrAssessment {
    PrAssessment {
        observation: obs.id.clone(),
        binding: PrBinding::Bound,
        outcome,
        summary,
        stale: false,
    }
}

/// Decide what `obs` means for `subject` at `now_ms`. `max_age_ms == 0` disables staleness.
pub fn assess(
    obs: &PrObservation,
    subject: Option<&ChangeSubject>,
    now_ms: i64,
    max_age_ms: i64,
) -> PrAssessment {
    let Some(subject) = subject else {
        let what = match obs.facts() {
            Some(f) => f.identity.key(),
            None => "pull request".into(),
        };
        return unbound(obs, PrUnbound::NoSubject, &what);
    };
    let mut a = match &obs.lookup {
        Lookup::Failed { reason } => bound(
            obs,
            EvidenceOutcome::Unknown,
            format!("Pull request lookup failed ({reason}); outcome unknown"),
        ),
        Lookup::NoPr => bound(
            obs,
            EvidenceOutcome::Unknown,
            "No pull request was found for this branch".into(),
        ),
        Lookup::Observed { facts } => assess_facts(obs, facts, subject),
    };
    if max_age_ms > 0
        && now_ms.saturating_sub(obs.observed_at_ms) > max_age_ms
        && matches!(a.outcome, EvidenceOutcome::Passed | EvidenceOutcome::Failed)
    {
        let mins = now_ms.saturating_sub(obs.observed_at_ms) / 60_000;
        a.outcome = EvidenceOutcome::Unknown;
        a.stale = true;
        a.summary = format!("{} (observed {mins} min ago; observe again)", a.summary);
    }
    a
}

fn assess_facts(obs: &PrObservation, f: &PrFacts, subject: &ChangeSubject) -> PrAssessment {
    let what = f.identity.key();
    match subject
        .repo
        .origin_url
        .as_deref()
        .and_then(normalize_remote)
    {
        None => return unbound(obs, PrUnbound::RepositoryUnverifiable, &what),
        Some(r) if r != f.identity.canonical_repo() => {
            return unbound(obs, PrUnbound::RepositoryMismatch, &what);
        }
        Some(_) => {}
    }
    if subject.kind != SubjectKind::Committed || subject.dirty_state != DirtyState::Clean {
        return unbound(obs, PrUnbound::SubjectNotCommitted, &what);
    }
    if !f.head_sha.eq_ignore_ascii_case(&subject.head_sha) {
        return unbound(obs, PrUnbound::HeadChanged, &what);
    }
    let short = &f.head_sha[..f.head_sha.len().min(10)];
    match f.state {
        PrState::Merged => unbound(obs, PrUnbound::Merged, &what),
        PrState::Closed => bound(
            obs,
            EvidenceOutcome::Failed,
            format!("{what} was closed without merging (head {short})"),
        ),
        PrState::Open => {
            if f.draft {
                return unbound(obs, PrUnbound::Draft, &what);
            }
            if f.checks == ChecksRollup::Failing {
                return bound(
                    obs,
                    EvidenceOutcome::Failed,
                    format!("{what} has failing checks (head {short})"),
                );
            }
            if f.review_decision.as_deref() == Some("CHANGES_REQUESTED") {
                return bound(
                    obs,
                    EvidenceOutcome::Failed,
                    format!("{what} has requested changes (head {short})"),
                );
            }
            if f.checks == ChecksRollup::Pending {
                return unbound(obs, PrUnbound::ChecksPending, &what);
            }
            bound(
                obs,
                EvidenceOutcome::Passed,
                format!(
                    "{what} is open for this revision (head {short}, into {})",
                    f.target_branch
                ),
            )
        }
    }
}

/// Evidence for the readiness engine. Bound only when the observation is for exactly this
/// subject; otherwise `subject_id` is `None` and the engine reports an older-revision
/// observation.
pub fn to_evidence(
    obs: &PrObservation,
    a: &PrAssessment,
    subject: Option<&ChangeSubject>,
) -> Evidence {
    Evidence {
        id: obs.id.clone(),
        category: EvidenceCategory::ExternalObservation,
        outcome: a.outcome,
        check_definition_id: None,
        definition_digest: None,
        environment_digest: None,
        subject_id: if a.is_bound() {
            subject.map(|s| s.id.clone())
        } else {
            None
        },
        criterion_ids: obs.criterion_ids.clone(),
        actor: Some(obs.observed_by.clone()),
        observed_at_ms: obs.observed_at_ms,
        summary: Some(a.summary.clone()),
    }
}

/// Ids of the newest observation per pull request (by PR key; failed lookups key on what was
/// requested), so a newer observation supersedes older ones in the display.
pub fn current_ids(obs: &[PrObservation]) -> BTreeSet<String> {
    let mut newest: BTreeMap<String, (&str, i64)> = BTreeMap::new();
    for o in obs {
        let key = o
            .key()
            .unwrap_or_else(|| format!("req:{}", o.requested.as_deref().unwrap_or("")));
        let e = newest.entry(key).or_insert((o.id.as_str(), i64::MIN));
        if o.observed_at_ms >= e.1 {
            *e = (o.id.as_str(), o.observed_at_ms);
        }
    }
    newest.values().map(|(id, _)| id.to_string()).collect()
}

/// The PR head moved between two observations of the same pull request.
pub fn head_changed(prev: &PrObservation, new: &PrObservation) -> bool {
    match (prev.facts(), new.facts()) {
        (Some(a), Some(b)) => a.identity.key() == b.identity.key() && a.head_sha != b.head_sha,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subject::{RepoIdentity, SubjectKind};

    const HEAD: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
    const OTHER: &str = "ffffffffffffffffffffffffffffffffffffffff";

    fn gh_json(state: &str, draft: bool, head: &str, roll: &str, decision: &str) -> String {
        format!(
            r#"{{"number":12,"url":"https://github.com/Acme/App/pull/12","state":"{state}","isDraft":{draft},"baseRefName":"main","headRefName":"feat","headRefOid":"{head}","reviewDecision":"{decision}","statusCheckRollup":{roll}}}"#
        )
    }

    fn subject(
        origin: Option<&str>,
        head: &str,
        kind: SubjectKind,
        dirty: DirtyState,
    ) -> ChangeSubject {
        ChangeSubject::new(
            RepoIdentity {
                root: "/r".into(),
                origin_url: origin.map(str::to_string),
            },
            "b".repeat(40),
            head.into(),
            None,
            dirty,
            kind,
            1,
        )
    }

    fn committed(head: &str) -> ChangeSubject {
        subject(
            Some("git@github.com:acme/app.git"),
            head,
            SubjectKind::Committed,
            DirtyState::Clean,
        )
    }

    fn obs_from(json: &str, at: i64) -> PrObservation {
        PrObservation {
            id: format!("o{at}"),
            task: "t1".into(),
            requested: None,
            lookup: Lookup::Observed {
                facts: parse_gh_pr_view(json).unwrap(),
            },
            observed_at_ms: at,
            authorization_scope: "gh_cli".into(),
            provider: "gh".into(),
            observed_by: Actor::user("me"),
            criterion_ids: vec!["c1".into()],
        }
    }

    #[test]
    fn urls_and_remotes_normalize() {
        let i = parse_pr_url("https://GitHub.com/Acme/App/pull/12/files?x=1#y").unwrap();
        assert_eq!(i.key(), "github.com/acme/app#12");
        assert_eq!(i.url, "https://github.com/Acme/App/pull/12");
        for bad in [
            "http://github.com/a/b/pull/1",
            "https://evil@github.com/a/b/pull/1",
            "https://github.com/a/b/issues/1",
            "https://github.com/a/b/pull/x",
            "https://github.com/a/b/pull/0",
            "https://github.com/a/b",
            "javascript:alert(1)",
        ] {
            assert!(parse_pr_url(bad).is_none(), "{bad}");
        }
        for u in [
            "git@github.com:acme/app.git",
            "https://github.com/acme/app",
            "https://user:tok@github.com/Acme/App.git/",
            "ssh://git@github.com:22/acme/app.git",
        ] {
            assert_eq!(
                normalize_remote(u).as_deref(),
                Some("github.com/acme/app"),
                "{u}"
            );
        }
        // A look-alike host never matches.
        assert_eq!(
            normalize_remote("https://evil.example/x@github.com/acme/app").as_deref(),
            None
        );
        assert!(normalize_remote("/local/path").is_none());
    }

    #[test]
    fn gh_output_is_parsed_strictly() {
        let f = parse_gh_pr_view(&gh_json("OPEN", false, HEAD, "[]", "")).unwrap();
        assert_eq!(f.target_branch, "main");
        assert_eq!(f.head_sha, HEAD);
        assert_eq!(f.state, PrState::Open);
        assert_eq!(f.checks, ChecksRollup::None);
        assert!(parse_gh_pr_view("nope").is_err());
        assert!(parse_gh_pr_view(&gh_json("OPEN", false, "abc", "[]", "")).is_err());
        assert!(parse_gh_pr_view(&gh_json("WEIRD", false, HEAD, "[]", "")).is_err());
        let mismatch =
            gh_json("OPEN", false, HEAD, "[]", "").replace("\"number\":12", "\"number\":13");
        assert!(parse_gh_pr_view(&mismatch).is_err());
        let pend = r#"[{"status":"IN_PROGRESS","conclusion":""}]"#;
        assert_eq!(
            parse_gh_pr_view(&gh_json("OPEN", false, HEAD, pend, ""))
                .unwrap()
                .checks,
            ChecksRollup::Pending
        );
        let bad = r#"[{"status":"COMPLETED","conclusion":"FAILURE"}]"#;
        assert_eq!(
            parse_gh_pr_view(&gh_json("OPEN", false, HEAD, bad, ""))
                .unwrap()
                .checks,
            ChecksRollup::Failing
        );
    }

    #[test]
    fn open_pr_at_the_subject_head_supports_the_criterion() {
        let o = obs_from(&gh_json("OPEN", false, HEAD, "[]", ""), 1000);
        let s = committed(HEAD);
        let a = assess(&o, Some(&s), 2000, 600_000);
        assert!(a.is_bound());
        assert_eq!(a.outcome, EvidenceOutcome::Passed);
        let e = to_evidence(&o, &a, Some(&s));
        assert_eq!(e.category, EvidenceCategory::ExternalObservation);
        assert_eq!(e.subject_id.as_deref(), Some(s.id.as_str()));
        assert_eq!(e.criterion_ids, vec!["c1".to_string()]);
    }

    #[test]
    fn changed_head_invalidates_the_observation_for_the_old_subject() {
        let o = obs_from(&gh_json("OPEN", false, HEAD, "[]", ""), 1000);
        // The branch moved on: the reviewed subject is now a different commit.
        let s = committed(OTHER);
        let a = assess(&o, Some(&s), 2000, 0);
        assert_eq!(
            a.binding,
            PrBinding::Unbound {
                reason: PrUnbound::HeadChanged
            }
        );
        assert_eq!(a.outcome, EvidenceOutcome::Unknown);
        assert!(to_evidence(&o, &a, Some(&s)).subject_id.is_none());
        // And a re-observation with the new head is a head change between rows.
        let n = obs_from(&gh_json("OPEN", false, OTHER, "[]", ""), 3000);
        assert!(head_changed(&o, &n));
        assert!(assess(&n, Some(&s), 3100, 0).is_bound());
    }

    #[test]
    fn failures_drafts_pending_and_merged() {
        let s = committed(HEAD);
        let ev = |j: String| assess(&obs_from(&j, 1000), Some(&s), 1500, 0);
        let bad = r#"[{"status":"COMPLETED","conclusion":"FAILURE"}]"#;
        let pend = r#"[{"status":"QUEUED","conclusion":""}]"#;
        assert_eq!(
            ev(gh_json("CLOSED", false, HEAD, "[]", "")).outcome,
            EvidenceOutcome::Failed
        );
        assert_eq!(
            ev(gh_json("OPEN", false, HEAD, bad, "")).outcome,
            EvidenceOutcome::Failed
        );
        assert_eq!(
            ev(gh_json("OPEN", false, HEAD, "[]", "CHANGES_REQUESTED")).outcome,
            EvidenceOutcome::Failed
        );
        for (j, why) in [
            (gh_json("OPEN", true, HEAD, "[]", ""), PrUnbound::Draft),
            (
                gh_json("OPEN", false, HEAD, pend, ""),
                PrUnbound::ChecksPending,
            ),
            (gh_json("MERGED", false, HEAD, "[]", ""), PrUnbound::Merged),
        ] {
            let a = ev(j);
            assert_eq!(a.binding, PrBinding::Unbound { reason: why });
            assert_eq!(a.outcome, EvidenceOutcome::Unknown);
        }
    }

    #[test]
    fn repository_must_match_and_dirty_work_is_not_in_a_pr() {
        let o = obs_from(&gh_json("OPEN", false, HEAD, "[]", ""), 1000);
        let other_repo = subject(
            Some("git@github.com:acme/other.git"),
            HEAD,
            SubjectKind::Committed,
            DirtyState::Clean,
        );
        assert_eq!(
            assess(&o, Some(&other_repo), 1500, 0).binding,
            PrBinding::Unbound {
                reason: PrUnbound::RepositoryMismatch
            }
        );
        let no_origin = subject(None, HEAD, SubjectKind::Committed, DirtyState::Clean);
        assert_eq!(
            assess(&o, Some(&no_origin), 1500, 0).binding,
            PrBinding::Unbound {
                reason: PrUnbound::RepositoryUnverifiable
            }
        );
        let dirty = subject(
            Some("git@github.com:acme/app.git"),
            HEAD,
            SubjectKind::DirtySnapshot,
            DirtyState::Dirty,
        );
        assert_eq!(
            assess(&o, Some(&dirty), 1500, 0).binding,
            PrBinding::Unbound {
                reason: PrUnbound::SubjectNotCommitted
            }
        );
        assert_eq!(
            assess(&o, None, 1500, 0).binding,
            PrBinding::Unbound {
                reason: PrUnbound::NoSubject
            }
        );
    }

    #[test]
    fn failed_and_offline_lookups_stay_unknown_and_stale_ones_are_demoted() {
        let s = committed(HEAD);
        let mut o = obs_from(&gh_json("OPEN", false, HEAD, "[]", ""), 0);
        o.lookup = Lookup::Failed {
            reason: "offline".into(),
        };
        let a = assess(&o, Some(&s), 10, 0);
        assert!(a.is_bound());
        assert_eq!(a.outcome, EvidenceOutcome::Unknown);
        o.lookup = Lookup::NoPr;
        assert_eq!(
            assess(&o, Some(&s), 10, 0).outcome,
            EvidenceOutcome::Unknown
        );

        let fresh = obs_from(&gh_json("OPEN", false, HEAD, "[]", ""), 0);
        assert_eq!(
            assess(&fresh, Some(&s), 59_000, 60_000).outcome,
            EvidenceOutcome::Passed
        );
        let st = assess(&fresh, Some(&s), 120_000, 60_000);
        assert!(st.stale);
        assert_eq!(st.outcome, EvidenceOutcome::Unknown);
    }

    #[test]
    fn claims_are_never_evidence() {
        let c = PrClaim::new(
            "k1".into(),
            "t1".into(),
            "https://github.com/acme/app/pull/12",
            ClaimSource::AgentStatement,
            Actor::agent("run1"),
            Some("PR opened".into()),
        );
        let e = c.to_evidence(&["c1".into()]);
        assert_eq!(e.category, EvidenceCategory::AgentClaim);
        assert_eq!(e.outcome, EvidenceOutcome::Unknown);
        assert!(e.subject_id.is_none());
        assert!(c.label().contains("not confirmed"));
        let junk = PrClaim::new(
            "k2".into(),
            "t1".into(),
            "see my PR",
            ClaimSource::PastedUrl,
            Actor::user("me"),
            None,
        );
        assert!(junk.identity.is_none());
    }

    #[test]
    fn newest_observation_per_pr_is_current() {
        let a = obs_from(&gh_json("OPEN", false, HEAD, "[]", ""), 100);
        let b = obs_from(&gh_json("OPEN", false, OTHER, "[]", ""), 200);
        let cur = current_ids(&[a.clone(), b.clone()]);
        assert!(cur.contains(&b.id) && !cur.contains(&a.id));
    }
}
