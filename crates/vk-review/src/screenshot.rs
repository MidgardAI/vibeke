//! Screenshots as evidence (spec 06 B6, 15 §6.4): the checkout identity captured when a
//! screenshot is taken ([`CodeState`]), the identity of the build that actually served the page
//! ([`RuntimeIdentity`]), and the binding rule between them ([`decide_binding`]).
//!
//! The two identities are deliberately separate. A checkout SHA observed at screenshot time
//! alone cannot bind an old server's response to new code, so a screenshot is `bound` only when
//! the running build reports exactly the checkout state Vibeke captured; otherwise it is
//! `illustrative` ("Build not verified") and can never satisfy a check criterion.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::subject::{self, ChangeSubject, DirtyState};

/// What code a screenshot shows, captured from the preview's checkout at screenshot time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeState {
    /// Absolute top-level path of the repository (worktree root).
    pub repo: String,
    #[serde(default)]
    pub origin_url: Option<String>,
    /// `None` for an unborn HEAD.
    pub head_sha: Option<String>,
    /// Digest over staged, unstaged and untracked content (same computation as the review
    /// observation baseline / live subject); `None` when the tree is clean **or** when capture
    /// failed — `dirty_state` tells which (`unknown` is never treated as clean).
    pub dirty_digest: Option<String>,
    pub dirty_state: DirtyState,
    pub captured_at_ms: i64,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl CodeState {
    /// Short label: `a1b2c3d` / `a1b2c3d+dirty` / `a1b2c3d (state unknown)`.
    pub fn label(&self) -> String {
        let head = self
            .head_sha
            .as_deref()
            .map(|h| &h[..h.len().min(7)])
            .unwrap_or("no commits");
        match self.dirty_state {
            DirtyState::Clean => head.to_string(),
            DirtyState::Dirty => format!("{head}+dirty"),
            DirtyState::Unknown => format!("{head} (state unknown)"),
        }
    }

    /// Does this state show exactly the content of `subject`? A committed subject matches a
    /// clean tree at its head; a live-checkout subject matches its head plus dirty digest.
    pub fn matches_subject(&self, subject: &ChangeSubject) -> bool {
        if self.head_sha.as_deref() != Some(subject.head_sha.as_str()) {
            return false;
        }
        match (self.dirty_state, subject.dirty_state) {
            (DirtyState::Clean, DirtyState::Clean) => subject.dirty_digest.is_none(),
            (DirtyState::Dirty, DirtyState::Dirty) => {
                self.dirty_digest.is_some() && self.dirty_digest == subject.dirty_digest
            }
            _ => false,
        }
    }
}

/// Capture the [`CodeState`] of the checkout containing `path` (read-only git plumbing:
/// `rev-parse`, `status --porcelain=v2 -z`, `diff --binary HEAD`, untracked file hashes).
/// Blocking; call it off the state actor / render path.
pub fn capture_code_state(path: &Path) -> Result<CodeState, subject::SubjectError> {
    let b = subject::observation_baseline(path)?;
    Ok(CodeState {
        repo: b.repo.root,
        origin_url: b.repo.origin_url,
        head_sha: b.head,
        dirty_digest: match b.dirty_state {
            DirtyState::Dirty => b.change_digest,
            _ => None,
        },
        dirty_state: b.dirty_state,
        captured_at_ms: b.observed_at_ms,
        warnings: b.warnings,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeStatus {
    /// The running build reported an identity.
    Known,
    /// No identity available: **Build not verified**.
    #[default]
    Unknown,
}

/// Identity of the build that served the page, as reported by the running app (e.g. a
/// `/__vibeke_build` endpoint serving the output of `vibeke screenshot code-state --json` that
/// its dev/build script captured at launch) or by the caller that took the screenshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RuntimeIdentity {
    pub status: RuntimeStatus,
    /// `probe` (`/__vibeke_build`), `header` (`X-Vibeke-Build`), `caller`, or `none`.
    pub source: String,
    /// Opaque build id / digest (Vite/Next build id, container digest …).
    pub build_id: Option<String>,
    /// Checkout state the running build was produced from, if it reports one.
    pub head_sha: Option<String>,
    pub dirty_digest: Option<String>,
    pub dirty_state: Option<DirtyState>,
    /// When the serving process/build started, if reported.
    pub started_at_ms: Option<i64>,
    /// Data fixture identity, if reported.
    pub fixture: Option<String>,
    pub observed_at_ms: i64,
    /// Why the identity is unknown (probe failed, not exposed …).
    pub detail: Option<String>,
}

impl RuntimeIdentity {
    pub fn unknown(detail: impl Into<String>, at_ms: i64) -> Self {
        RuntimeIdentity {
            status: RuntimeStatus::Unknown,
            source: "none".into(),
            detail: Some(detail.into()),
            observed_at_ms: at_ms,
            ..Default::default()
        }
    }

    /// Parse a build report: the JSON of `vibeke screenshot code-state --json` (a
    /// [`CodeState`]) or `{build_id?, head_sha?, dirty_digest?, dirty_state?, started_at_ms?,
    /// fixture?}`. Returns `None` when it carries no identity at all.
    pub fn from_report(v: &serde_json::Value, source: &str, at_ms: i64) -> Option<Self> {
        let str_of = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let head_sha = str_of("head_sha").filter(|s| !s.is_empty());
        let build_id = str_of("build_id")
            .or_else(|| str_of("buildId"))
            .filter(|s| !s.is_empty());
        if head_sha.is_none() && build_id.is_none() {
            return None;
        }
        let dirty_state = v
            .get("dirty_state")
            .and_then(|x| serde_json::from_value::<DirtyState>(x.clone()).ok());
        Some(RuntimeIdentity {
            status: RuntimeStatus::Known,
            source: source.into(),
            build_id,
            head_sha,
            dirty_digest: str_of("dirty_digest").filter(|s| !s.is_empty()),
            dirty_state,
            started_at_ms: v
                .get("started_at_ms")
                .and_then(|x| x.as_i64())
                .or_else(|| v.get("captured_at_ms").and_then(|x| x.as_i64())),
            fixture: str_of("fixture"),
            observed_at_ms: at_ms,
            detail: None,
        })
    }

    /// Parse an `X-Vibeke-Build: <head_sha>[+<dirty_digest>][; build=<id>]` header value.
    pub fn from_header(value: &str, at_ms: i64) -> Option<Self> {
        let mut parts = value.split(';').map(str::trim);
        let first = parts.next().unwrap_or("");
        let (head, dirty) = match first.split_once('+') {
            Some((h, d)) => (h, Some(d)),
            None => (first, None),
        };
        let mut build_id = None;
        for p in parts {
            if let Some(b) = p.strip_prefix("build=") {
                build_id = Some(b.to_string());
            }
        }
        let head_ok = !head.is_empty() && head.chars().all(|c| c.is_ascii_hexdigit());
        if !head_ok && build_id.is_none() {
            return None;
        }
        Some(RuntimeIdentity {
            status: RuntimeStatus::Known,
            source: "header".into(),
            build_id,
            head_sha: head_ok.then(|| head.to_string()),
            dirty_digest: dirty.filter(|d| !d.is_empty()).map(str::to_string),
            dirty_state: head_ok.then_some(if dirty.is_some() {
                DirtyState::Dirty
            } else {
                DirtyState::Clean
            }),
            observed_at_ms: at_ms,
            ..Default::default()
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Binding {
    /// The running build reported exactly the checkout state captured at screenshot time.
    Bound,
    /// Everything else: the image illustrates, it does not verify (15 §6.4).
    Illustrative,
}

/// Bound only when the running build's reported identity equals the captured checkout state
/// (head SHA, and dirty digest when dirty / none when clean). Returns the binding and a reason
/// fit for the UI.
pub fn decide_binding(code: Option<&CodeState>, runtime: &RuntimeIdentity) -> (Binding, String) {
    let Some(code) = code else {
        return (
            Binding::Illustrative,
            "No code state: the preview isn't tied to a repository checkout".into(),
        );
    };
    if code.dirty_state == DirtyState::Unknown || code.head_sha.is_none() {
        return (
            Binding::Illustrative,
            "Checkout state could not be captured completely".into(),
        );
    }
    if runtime.status != RuntimeStatus::Known {
        return (
            Binding::Illustrative,
            "Build not verified: the running app reported no build identity".into(),
        );
    }
    let Some(rt_head) = runtime.head_sha.as_deref() else {
        return (
            Binding::Illustrative,
            format!(
                "Build not verified: build id {} is not tied to a checkout state",
                runtime.build_id.as_deref().unwrap_or("?")
            ),
        );
    };
    if Some(rt_head) != code.head_sha.as_deref() {
        return (
            Binding::Illustrative,
            format!(
                "Running build is {} but the checkout is at {}",
                &rt_head[..rt_head.len().min(7)],
                code.label()
            ),
        );
    }
    let rt_dirty = runtime.dirty_digest.as_deref();
    let rt_dirty_state = runtime.dirty_state.unwrap_or(if rt_dirty.is_some() {
        DirtyState::Dirty
    } else {
        DirtyState::Clean
    });
    let same = match code.dirty_state {
        DirtyState::Clean => rt_dirty_state == DirtyState::Clean && rt_dirty.is_none(),
        DirtyState::Dirty => {
            rt_dirty_state == DirtyState::Dirty && rt_dirty == code.dirty_digest.as_deref()
        }
        DirtyState::Unknown => false,
    };
    if same {
        (
            Binding::Bound,
            format!("Running build matches the checkout ({})", code.label()),
        )
    } else {
        (
            Binding::Illustrative,
            format!(
                "Running build was produced from different uncommitted changes than the checkout ({})",
                code.label()
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subject::testrepo::TestRepo;
    use crate::subject::{RepoIdentity, SubjectKind};

    fn runtime_from(code: &CodeState) -> RuntimeIdentity {
        RuntimeIdentity::from_report(&serde_json::to_value(code).unwrap(), "probe", 1).unwrap()
    }

    #[test]
    fn code_state_clean_and_dirty() {
        let r = TestRepo::new();
        r.write("a.txt", "one\n");
        let head = r.commit("init");
        let clean = capture_code_state(&r.path()).unwrap();
        assert_eq!(clean.head_sha.as_deref(), Some(head.as_str()));
        assert_eq!(clean.dirty_state, DirtyState::Clean);
        assert_eq!(clean.dirty_digest, None);
        assert_eq!(clean.repo, r.path().to_string_lossy());
        assert_eq!(clean.label(), head[..7]);

        // A subdirectory resolves to the same repository.
        std::fs::create_dir_all(r.root().join("sub")).unwrap();
        let from_sub = capture_code_state(&r.root().join("sub")).unwrap();
        assert_eq!(from_sub.repo, clean.repo);

        r.write("a.txt", "two\n");
        let d1 = capture_code_state(&r.path()).unwrap();
        assert_eq!(d1.dirty_state, DirtyState::Dirty);
        assert!(d1.dirty_digest.is_some());
        assert_eq!(d1.head_sha, clean.head_sha);
        assert!(d1.label().ends_with("+dirty"));
        // Same content → same digest; an untracked file changes it.
        assert_eq!(
            capture_code_state(&r.path()).unwrap().dirty_digest,
            d1.dirty_digest
        );
        r.write("new.txt", "x");
        let d2 = capture_code_state(&r.path()).unwrap();
        assert_ne!(d2.dirty_digest, d1.dirty_digest);

        // Not a repository.
        let t = tempfile::tempdir().unwrap();
        assert!(capture_code_state(t.path()).is_err());
    }

    #[test]
    fn binding_bound_only_when_the_running_build_matches() {
        let r = TestRepo::new();
        r.write("a.txt", "one\n");
        r.commit("init");
        let clean = capture_code_state(&r.path()).unwrap();

        // Unknown runtime → illustrative ("Build not verified").
        let (b, why) = decide_binding(Some(&clean), &RuntimeIdentity::unknown("no probe", 1));
        assert_eq!(b, Binding::Illustrative);
        assert!(why.contains("Build not verified"), "{why}");
        // No code state → illustrative.
        assert_eq!(
            decide_binding(None, &runtime_from(&clean)).0,
            Binding::Illustrative
        );
        // Exact match → bound.
        let (b, why) = decide_binding(Some(&clean), &runtime_from(&clean));
        assert_eq!(b, Binding::Bound, "{why}");

        // Build id only → illustrative.
        let rt = RuntimeIdentity::from_report(&serde_json::json!({"build_id": "abc"}), "probe", 1)
            .unwrap();
        assert_eq!(decide_binding(Some(&clean), &rt).0, Binding::Illustrative);

        // The checkout moved on (new commit) while the server still runs the old build.
        let old_runtime = runtime_from(&clean);
        r.write("a.txt", "two\n");
        r.commit("second");
        let moved = capture_code_state(&r.path()).unwrap();
        let (b, why) = decide_binding(Some(&moved), &old_runtime);
        assert_eq!(b, Binding::Illustrative);
        assert!(why.contains("Running build is"), "{why}");

        // Dirty: bound only with the same dirty digest.
        r.write("a.txt", "three\n");
        let dirty = capture_code_state(&r.path()).unwrap();
        assert_eq!(
            decide_binding(Some(&dirty), &runtime_from(&dirty)).0,
            Binding::Bound
        );
        assert_eq!(
            decide_binding(Some(&dirty), &runtime_from(&moved)).0,
            Binding::Illustrative
        );
        let mut other = runtime_from(&dirty);
        other.dirty_digest = Some("ffff".into());
        assert_eq!(
            decide_binding(Some(&dirty), &other).0,
            Binding::Illustrative
        );
        // Unknown checkout state never binds.
        let mut unk = dirty.clone();
        unk.dirty_state = DirtyState::Unknown;
        unk.dirty_digest = None;
        assert_eq!(
            decide_binding(Some(&unk), &runtime_from(&unk)).0,
            Binding::Illustrative
        );
    }

    #[test]
    fn header_and_subject_matching() {
        let h = RuntimeIdentity::from_header("0123abcd+deadbeef; build=42", 5).unwrap();
        assert_eq!(h.head_sha.as_deref(), Some("0123abcd"));
        assert_eq!(h.dirty_digest.as_deref(), Some("deadbeef"));
        assert_eq!(h.dirty_state, Some(DirtyState::Dirty));
        assert_eq!(h.build_id.as_deref(), Some("42"));
        assert!(RuntimeIdentity::from_header("not hex!", 5).is_none());
        assert!(RuntimeIdentity::from_report(&serde_json::json!({}), "probe", 1).is_none());

        let repo = RepoIdentity {
            root: "/r".into(),
            origin_url: None,
        };
        let committed = ChangeSubject::new(
            repo.clone(),
            "b".repeat(40),
            "a".repeat(40),
            None,
            DirtyState::Clean,
            SubjectKind::Committed,
            0,
        );
        let mut code = CodeState {
            repo: "/r".into(),
            origin_url: None,
            head_sha: Some("a".repeat(40)),
            dirty_digest: None,
            dirty_state: DirtyState::Clean,
            captured_at_ms: 0,
            warnings: vec![],
        };
        assert!(code.matches_subject(&committed));
        code.dirty_state = DirtyState::Dirty;
        code.dirty_digest = Some("d1".into());
        assert!(!code.matches_subject(&committed));
        let live = ChangeSubject::new(
            repo,
            "b".repeat(40),
            "a".repeat(40),
            Some("d1".into()),
            DirtyState::Dirty,
            SubjectKind::CheckoutLive,
            0,
        );
        assert!(code.matches_subject(&live));
        code.head_sha = Some("c".repeat(40));
        assert!(!code.matches_subject(&live));
    }
}
