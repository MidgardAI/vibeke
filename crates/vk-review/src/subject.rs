//! Change subjects, observation baselines and review-base proposals (§5).
//!
//! Every git invocation here is read-only with respect to the user's checkout (no stash, reset,
//! checkout, clean or index refresh) and bounded by a timeout. Callers run these off the
//! terminal/state-actor hot path (e.g. on a blocking thread).

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::gitcmd::{self, GitError};
use crate::{FieldHasher, now_ms, truncate_utf8};

#[derive(Debug, thiserror::Error)]
pub enum SubjectError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("{0} does not resolve to a commit")]
    BadRevision(String),
    #[error("repository has no commits yet (unborn HEAD)")]
    UnbornHead,
    #[error("subject {0} is not an immutable committed subject")]
    NotImmutable(String),
    #[error("subject id does not match its identity fields")]
    IdMismatch,
    #[error("snapshot {0} does not match its recorded tree/parent")]
    SnapshotMismatch(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoIdentity {
    /// Absolute top-level path of the repository (worktree root).
    pub root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirtyState {
    Clean,
    Dirty,
    /// Capture data missing; never treated as clean.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    /// `base..head` of immutable commits. Accept-capable in T2.
    Committed,
    /// The live checkout (head plus a dirty digest). Inspect-only in T2.
    CheckoutLive,
    /// A validated, content-addressed snapshot of dirty work (staged + unstaged + untracked,
    /// binary included) stored as an immutable Git commit under `refs/vibeke/snapshots/`
    /// (15 §5, T4). Accept-capable and verifiable like a committed subject.
    DirtySnapshot,
}

/// Where a [`SubjectKind::DirtySnapshot`]'s content lives: an immutable commit (parent = the
/// checkout's HEAD at capture) in the repository's object store, kept reachable by a
/// Vibeke-private ref (`refs/vibeke/snapshots/<commit>`, removed again by snapshot GC once
/// nothing references it). The user's index, worktree, branches and tags are never touched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRef {
    /// Snapshot commit; its tree is the full working-tree content.
    pub commit: String,
    pub tree: String,
    /// Tree of the user's index at capture (the staged part), for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_tree: Option<String>,
    /// `refs/vibeke/snapshots/<commit>`.
    pub ref_name: String,
    /// Capture attempts needed until the before/after digests agreed.
    #[serde(default)]
    pub attempts: u32,
}

/// Immutable, content-addressed description of what is under review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSubject {
    /// blake3 over repo identity, base, head, dirty digest/state and kind (not capture time).
    pub id: String,
    pub repo: RepoIdentity,
    pub base_sha: String,
    pub head_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty_digest: Option<String>,
    pub dirty_state: DirtyState,
    pub captured_at_ms: i64,
    pub kind: SubjectKind,
    /// Immutable content of a dirty snapshot (`kind = dirty_snapshot` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotRef>,
}

impl ChangeSubject {
    pub fn new(
        repo: RepoIdentity,
        base_sha: String,
        head_sha: String,
        dirty_digest: Option<String>,
        dirty_state: DirtyState,
        kind: SubjectKind,
        captured_at_ms: i64,
    ) -> Self {
        let id = Self::compute_id(
            &repo,
            &base_sha,
            &head_sha,
            dirty_digest.as_deref(),
            dirty_state,
            kind,
        );
        ChangeSubject {
            id,
            repo,
            base_sha,
            head_sha,
            dirty_digest,
            dirty_state,
            captured_at_ms,
            kind,
            snapshot: None,
        }
    }

    /// A dirty-snapshot subject: `base..snapshot.commit`, with the live checkout's HEAD and
    /// change digest at capture.
    pub fn dirty_snapshot(
        repo: RepoIdentity,
        base_sha: String,
        head_sha: String,
        dirty_digest: String,
        snapshot: SnapshotRef,
        captured_at_ms: i64,
    ) -> Self {
        let mut s = ChangeSubject::new(
            repo,
            base_sha,
            head_sha,
            Some(dirty_digest),
            DirtyState::Dirty,
            SubjectKind::DirtySnapshot,
            captured_at_ms,
        );
        s.snapshot = Some(snapshot);
        s.id = s.identity();
        s
    }

    /// Content address over every identity field (not capture time or attempt count).
    fn identity(&self) -> String {
        let base = Self::compute_id(
            &self.repo,
            &self.base_sha,
            &self.head_sha,
            self.dirty_digest.as_deref(),
            self.dirty_state,
            self.kind,
        );
        match &self.snapshot {
            None => base,
            Some(sn) => {
                let mut h = FieldHasher::new("vk-review/change-subject/snapshot/v1");
                h.str(&base).str(&sn.commit).str(&sn.tree);
                h.finish()
            }
        }
    }

    pub fn compute_id(
        repo: &RepoIdentity,
        base_sha: &str,
        head_sha: &str,
        dirty_digest: Option<&str>,
        dirty_state: DirtyState,
        kind: SubjectKind,
    ) -> String {
        let mut h = FieldHasher::new("vk-review/change-subject/v1");
        h.str(&repo.root)
            .opt(repo.origin_url.as_deref())
            .str(base_sha)
            .str(head_sha)
            .opt(dirty_digest)
            .str(match dirty_state {
                DirtyState::Clean => "clean",
                DirtyState::Dirty => "dirty",
                DirtyState::Unknown => "unknown",
            })
            .str(match kind {
                SubjectKind::Committed => "committed",
                SubjectKind::CheckoutLive => "checkout_live",
                SubjectKind::DirtySnapshot => "dirty_snapshot",
            });
        h.finish()
    }

    /// The stored id matches the identity fields (detects tampering/corruption).
    pub fn verify_id(&self) -> bool {
        self.id == self.identity()
    }

    /// A committed subject (`base..head` of commits).
    pub fn is_committed(&self) -> bool {
        self.kind == SubjectKind::Committed
    }

    /// Accept-capable and verifiable: a committed subject (T2) or a validated dirty snapshot
    /// whose immutable content is recorded (T4, §5). The live checkout never is.
    pub fn is_immutable(&self) -> bool {
        match self.kind {
            SubjectKind::Committed => true,
            SubjectKind::DirtySnapshot => self.snapshot.is_some(),
            SubjectKind::CheckoutLive => false,
        }
    }

    /// The commit whose tree is this subject's content: the snapshot commit of a dirty
    /// snapshot, else `head_sha`. Diffs, check definitions and disposable checkouts use it.
    pub fn content_sha(&self) -> &str {
        self.snapshot
            .as_ref()
            .map(|s| s.commit.as_str())
            .unwrap_or(&self.head_sha)
    }

    /// Short head for display.
    pub fn short_head(&self) -> &str {
        &self.head_sha[..self.head_sha.len().min(10)]
    }
}

/// Resolve a revision to a full commit SHA (`git rev-parse --verify <rev>^{commit}`).
pub fn rev_parse(repo: &Path, rev: &str) -> Result<String, SubjectError> {
    if rev.starts_with('-') {
        return Err(SubjectError::BadRevision(rev.to_string()));
    }
    let spec = format!("{rev}^{{commit}}");
    let out = gitcmd::run_raw(
        repo,
        &["rev-parse", "--verify", "-q", "--end-of-options", &spec],
        gitcmd::GIT_TIMEOUT,
    )?;
    if out.code == Some(0) {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(SubjectError::BadRevision(rev.to_string()))
    }
}

/// `git merge-base --is-ancestor ancestor descendant`.
pub fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool, SubjectError> {
    let a = rev_parse(repo, ancestor)?;
    let d = rev_parse(repo, descendant)?;
    let out = gitcmd::run_raw(
        repo,
        &["merge-base", "--is-ancestor", &a, &d],
        gitcmd::GIT_TIMEOUT,
    )?;
    match out.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        code => Err(GitError::Failed {
            args: "merge-base --is-ancestor".into(),
            code,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        }
        .into()),
    }
}

/// Repository identity: top-level path and `remote.origin.url` if configured.
pub fn repo_identity(repo: &Path) -> Result<RepoIdentity, SubjectError> {
    let root = gitcmd::run(repo, &["rev-parse", "--show-toplevel"])?;
    let origin = gitcmd::run_raw(
        repo,
        &["config", "--get", "remote.origin.url"],
        gitcmd::GIT_TIMEOUT,
    )?;
    let origin_url = (origin.code == Some(0))
        .then(|| String::from_utf8_lossy(&origin.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(RepoIdentity { root, origin_url })
}

/// Capture an immutable committed subject `base_ref..head_ref`.
pub fn capture_committed(
    repo: &Path,
    base_ref: &str,
    head_ref: &str,
) -> Result<ChangeSubject, SubjectError> {
    let identity = repo_identity(repo)?;
    let base = rev_parse(repo, base_ref)?;
    let head = rev_parse(repo, head_ref)?;
    Ok(ChangeSubject::new(
        identity,
        base,
        head,
        None,
        DirtyState::Clean,
        SubjectKind::Committed,
        now_ms(),
    ))
}

/// What existed when tracking began (§5). Attribution metadata only; it does not prove who
/// made the changes and is not the default review base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    pub repo: RepoIdentity,
    /// `None` for an unborn HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Digest over staged, unstaged and untracked content; `None` when capture failed (then
    /// `dirty_state` is `unknown`, never `clean`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_digest: Option<String>,
    pub dirty_state: DirtyState,
    pub observed_at_ms: i64,
    /// Changes existed at tracking time; label **May include preexisting changes**.
    pub may_include_preexisting_changes: bool,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Submodules and nested repositories with changes of their own (a moved gitlink, or
    /// modified/untracked content inside). Their state is part of `change_digest`; a dirty
    /// snapshot can't capture them and refuses (`unsupported_capture`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_submodules: Vec<String>,
}

/// Capture an observation baseline: `git status --porcelain=v2 -z --untracked-files=all`,
/// `git diff --binary HEAD`, the contents and executable bit of every untracked file, and —
/// recursively — the HEAD and change digest of every submodule or nested repository that has
/// changes, hashed together. Missing capture data yields `change_digest: None` /
/// `DirtyState::Unknown`.
pub fn observation_baseline(repo: &Path) -> Result<Baseline, SubjectError> {
    baseline_at(repo, 0)
}

/// How deep submodules / nested repositories are followed into for the digest.
const MAX_NESTING: u32 = 8;

fn baseline_at(repo: &Path, depth: u32) -> Result<Baseline, SubjectError> {
    let identity = repo_identity(repo)?;
    let root = Path::new(&identity.root).to_path_buf();
    let head = rev_parse(&root, "HEAD").ok();
    let mut warnings = Vec::new();

    let digest = (|| -> Result<(String, bool, Vec<String>), String> {
        let status = gitcmd::run_bytes(
            &root,
            &[
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
        )
        .map_err(|e| e.to_string())?;
        let mut h = FieldHasher::new("vk-review/baseline/v2");
        h.field(&status);
        let dirty = !status.is_empty();
        if head.is_some() {
            let diff = gitcmd::run_bytes(
                &root,
                &[
                    "diff",
                    "--binary",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "HEAD",
                ],
            )
            .map_err(|e| e.to_string())?;
            h.field(&diff);
        } else {
            let diff = gitcmd::run_bytes(
                &root,
                &[
                    "diff",
                    "--binary",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--cached",
                ],
            )
            .map_err(|e| e.to_string())?;
            h.field(&diff);
        }
        let mut nested = Vec::new();
        for path in untracked_paths(&status) {
            // With `--untracked-files=all` Git lists a directory only when it is an embedded
            // repository it does not descend into: its content is followed like a submodule's.
            if let Some(dir) = path.strip_suffix('/') {
                h.str("nested").str(dir);
                hash_nested(&mut h, &root, dir, depth)?;
                nested.push(dir.to_string());
                continue;
            }
            h.str(&path);
            let full = root.join(&path);
            match std::fs::symlink_metadata(&full) {
                Ok(m) if m.file_type().is_symlink() => {
                    let target = std::fs::read_link(&full).map_err(|e| format!("{path}: {e}"))?;
                    h.str("symlink").str(&target.to_string_lossy());
                }
                Ok(m) if m.is_file() => {
                    let mut f = std::fs::File::open(&full).map_err(|e| format!("{path}: {e}"))?;
                    let mut fh = blake3::Hasher::new();
                    let mut buf = [0u8; 64 * 1024];
                    loop {
                        let n = f.read(&mut buf).map_err(|e| format!("{path}: {e}"))?;
                        if n == 0 {
                            break;
                        }
                        fh.update(&buf[..n]);
                    }
                    // The mode Git records (100755 vs 100644): `chmod +x` changes the subject.
                    h.str(if is_executable(&m) { "file+x" } else { "file" })
                        .str(&fh.finalize().to_hex());
                }
                Ok(_) => {
                    h.str("other");
                }
                Err(e) => return Err(format!("{path}: {e}")),
            }
        }
        // A submodule appears in the superproject's status/diff only as its commit (plus a
        // `-dirty` marker): edits inside an already-dirty submodule would leave both unchanged,
        // so its own state is part of the digest.
        for path in submodule_paths(&status) {
            h.str("submodule").str(&path);
            hash_nested(&mut h, &root, &path, depth)?;
            nested.push(path);
        }
        Ok((h.finish(), dirty, nested))
    })();

    let (change_digest, dirty_state, changed_submodules) = match digest {
        Ok((d, dirty, nested)) => (
            Some(d),
            if dirty {
                DirtyState::Dirty
            } else {
                DirtyState::Clean
            },
            nested,
        ),
        Err(e) => {
            warnings.push(format!("change capture incomplete: {e}"));
            (None, DirtyState::Unknown, vec![])
        }
    };
    if head.is_none() {
        warnings.push("repository has no commits yet".into());
    }
    Ok(Baseline {
        repo: identity,
        head,
        change_digest,
        dirty_state,
        observed_at_ms: now_ms(),
        may_include_preexisting_changes: dirty_state != DirtyState::Clean,
        warnings,
        changed_submodules,
    })
}

#[cfg(unix)]
fn is_executable(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // Git records a file as executable when its owner-execute bit is set.
    m.permissions().mode() & 0o100 != 0
}

#[cfg(not(unix))]
fn is_executable(_m: &std::fs::Metadata) -> bool {
    false
}

/// Fold a submodule's / nested repository's own HEAD and change digest into `h`.
fn hash_nested(h: &mut FieldHasher, root: &Path, rel: &str, depth: u32) -> Result<(), String> {
    if depth >= MAX_NESTING {
        return Err(format!(
            "{rel}: repositories nested deeper than {MAX_NESTING} levels"
        ));
    }
    let canon = root.join(rel).canonicalize().ok();
    let sub = canon.as_ref().and_then(|c| {
        baseline_at(c, depth + 1)
            .ok()
            // An uninitialized submodule directory resolves to the superproject: not its own.
            .filter(|b| Path::new(&b.repo.root).canonicalize().ok().as_ref() == Some(c))
    });
    match sub {
        Some(b) => {
            let Some(d) = b.change_digest.as_deref() else {
                return Err(format!("{rel}: {}", b.warnings.join("; ")));
            };
            h.str("repo").opt(b.head.as_deref()).str(d);
        }
        None => {
            h.str("uninitialized");
        }
    }
    Ok(())
}

/// Paths of submodule entries (`<sub>` field `S...`) in `git status --porcelain=v2 -z` output:
/// a changed gitlink, or modified/untracked content inside the submodule.
fn submodule_paths(status: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut fields = status.split(|b| *b == 0);
    while let Some(rec) = fields.next() {
        let Some(&kind) = rec.first() else {
            continue;
        };
        // Space-separated fields up to and including the path: `1` 9, `2` 10 (the original
        // path follows as its own NUL field), `u` 11.
        let n = match kind {
            b'1' => 9,
            b'2' => 10,
            b'u' => 11,
            _ => continue,
        };
        let parts: Vec<&[u8]> = rec.splitn(n, |b| *b == b' ').collect();
        if parts.len() == n && parts[2].first() == Some(&b'S') {
            out.push(String::from_utf8_lossy(parts[n - 1]).into_owned());
        }
        if kind == b'2' {
            fields.next();
        }
    }
    out
}

/// Paths of untracked entries (`? <path>`) in `git status --porcelain=v2 -z` output.
fn untracked_paths(status: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut fields = status.split(|b| *b == 0).peekable();
    while let Some(rec) = fields.next() {
        if rec.is_empty() {
            continue;
        }
        match rec[0] {
            b'?' if rec.len() > 2 => out.push(String::from_utf8_lossy(&rec[2..]).into_owned()),
            // Rename/copy records are followed by the original path as a separate field.
            b'2' => {
                fields.next();
            }
            _ => {}
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BaseReason {
    /// Owned task: its recorded resolved base.
    OwnedTaskBase { base_ref: String },
    /// Merge-base of HEAD with the user-selected target branch.
    MergeBaseWithTarget { branch: String },
    /// Merge-base of HEAD with the configured, locally resolvable default branch.
    MergeBaseWithDefault { branch: String },
    /// Fallback: current HEAD. Omits earlier committed work.
    HeadFallback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseProposal {
    pub base_sha: String,
    pub reason: BaseReason,
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Propose a review base (§5). Never fetches; only locally resolvable refs are used. The user
/// confirms after seeing the full diff.
pub fn propose_review_base(
    repo: &Path,
    owned_base: Option<&str>,
    target_branch: Option<&str>,
    default_branch: Option<&str>,
) -> Result<BaseProposal, SubjectError> {
    let head = rev_parse(repo, "HEAD").map_err(|_| SubjectError::UnbornHead)?;
    let mut warnings = Vec::new();
    if let Some(b) = owned_base {
        match rev_parse(repo, b) {
            Ok(sha) => {
                return Ok(BaseProposal {
                    base_sha: sha,
                    reason: BaseReason::OwnedTaskBase {
                        base_ref: b.to_string(),
                    },
                    warnings,
                });
            }
            Err(_) => warnings.push(format!("recorded task base {b} is not resolvable locally")),
        }
    }
    let candidates = [
        (target_branch, true),
        (default_branch.filter(|d| Some(*d) != target_branch), false),
    ];
    for (branch, is_target) in candidates {
        let Some(branch) = branch else { continue };
        let Ok(tip) = rev_parse(repo, branch) else {
            warnings.push(format!("branch {branch} is not resolvable locally"));
            continue;
        };
        match gitcmd::run(repo, &["merge-base", &head, &tip]) {
            Ok(mb) if !mb.is_empty() => {
                let reason = if is_target {
                    BaseReason::MergeBaseWithTarget {
                        branch: branch.to_string(),
                    }
                } else {
                    BaseReason::MergeBaseWithDefault {
                        branch: branch.to_string(),
                    }
                };
                return Ok(BaseProposal {
                    base_sha: mb,
                    reason,
                    warnings,
                });
            }
            _ => warnings.push(format!("no merge-base between HEAD and {branch}")),
        }
    }
    warnings.push("Review base is current HEAD: earlier committed work is omitted".into());
    Ok(BaseProposal {
        base_sha: head,
        reason: BaseReason::HeadFallback,
        warnings,
    })
}

/// The checkout's current branch (`None` when detached or unborn).
pub fn current_branch(repo: &Path) -> Option<String> {
    gitcmd::run(repo, &["symbolic-ref", "-q", "--short", "HEAD"])
        .ok()
        .filter(|b| !b.is_empty())
}

/// The repository's default branch, resolved locally without fetching (§5): the target of
/// `origin/HEAD` (the local branch of that name when it exists, else the remote-tracking ref),
/// otherwise a local `main` or `master`.
pub fn default_branch(repo: &Path) -> Option<String> {
    let local = |b: &str| rev_parse(repo, &format!("refs/heads/{b}")).is_ok();
    if let Ok(r) = gitcmd::run(
        repo,
        &["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"],
    ) && let Some(b) = r.strip_prefix("origin/")
        && !b.is_empty()
    {
        if local(b) {
            return Some(b.to_string());
        }
        if rev_parse(repo, &format!("refs/remotes/{r}")).is_ok() {
            return Some(r);
        }
    }
    ["main", "master"]
        .into_iter()
        .find(|b| local(b))
        .map(str::to_string)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    pub path: String,
    /// `None` for binary files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed: Option<u64>,
    /// Original path of a rename/copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DiffStat {
    pub files: Vec<FileStat>,
    pub added: u64,
    pub removed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffText {
    pub text: String,
    pub truncated: bool,
    pub total_bytes: usize,
}

fn require_immutable(subject: &ChangeSubject) -> Result<(), SubjectError> {
    if !subject.verify_id() {
        return Err(SubjectError::IdMismatch);
    }
    if !subject.is_immutable() {
        return Err(SubjectError::NotImmutable(subject.id.clone()));
    }
    let repo = Path::new(&subject.repo.root);
    for sha in [&subject.base_sha, &subject.head_sha] {
        let spec = format!("{sha}^{{commit}}");
        let out = gitcmd::run_raw(repo, &["cat-file", "-e", &spec], gitcmd::GIT_TIMEOUT)?;
        if out.code != Some(0) {
            return Err(SubjectError::BadRevision(sha.clone()));
        }
    }
    if let Some(sn) = &subject.snapshot {
        verify_snapshot(repo, &subject.head_sha, sn)?;
    }
    Ok(())
}

/// The snapshot commit exists, has exactly the recorded tree and the capture-time HEAD as its
/// only parent (so its content is what was validated, not something rewritten later).
pub fn verify_snapshot(repo: &Path, head_sha: &str, sn: &SnapshotRef) -> Result<(), SubjectError> {
    let out = gitcmd::run_raw(
        repo,
        &["cat-file", "commit", &sn.commit],
        gitcmd::GIT_TIMEOUT,
    )?;
    if out.code != Some(0) {
        return Err(SubjectError::BadRevision(sn.commit.clone()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let header: Vec<&str> = text.lines().take_while(|l| !l.is_empty()).collect();
    let tree_ok = header.iter().any(|l| *l == format!("tree {}", sn.tree));
    let parents: Vec<&str> = header
        .iter()
        .filter_map(|l| l.strip_prefix("parent "))
        .collect();
    if !tree_ok || parents != [head_sha] {
        return Err(SubjectError::SnapshotMismatch(sn.commit.clone()));
    }
    Ok(())
}

const DIFF_FLAGS: [&str; 4] = ["--no-color", "--no-ext-diff", "--no-textconv", "-M"];

/// `git diff --numstat base head` from immutable objects (never the live tree).
pub fn diff_stat(subject: &ChangeSubject) -> Result<DiffStat, SubjectError> {
    require_immutable(subject)?;
    let repo = Path::new(&subject.repo.root);
    let mut args = vec!["diff", "--numstat", "-z"];
    args.extend(DIFF_FLAGS);
    args.extend([subject.base_sha.as_str(), subject.content_sha()]);
    let out = gitcmd::run_bytes(repo, &args)?;
    Ok(parse_numstat_z(&out))
}

fn parse_numstat_z(out: &[u8]) -> DiffStat {
    let mut stat = DiffStat::default();
    let mut fields = out.split(|b| *b == 0);
    while let Some(rec) = fields.next() {
        if rec.is_empty() {
            continue;
        }
        let s = String::from_utf8_lossy(rec);
        let mut parts = s.splitn(3, '\t');
        let added = parts.next().and_then(|a| a.parse::<u64>().ok());
        let removed = parts.next().and_then(|a| a.parse::<u64>().ok());
        let rest = parts.next().unwrap_or("");
        let (path, renamed_from) = if rest.is_empty() {
            // Rename: "<a>\t<r>\t\0<from>\0<to>\0"
            let from = fields
                .next()
                .map(|f| String::from_utf8_lossy(f).into_owned());
            let to = fields
                .next()
                .map(|f| String::from_utf8_lossy(f).into_owned())
                .unwrap_or_default();
            (to, from)
        } else {
            (rest.to_string(), None)
        };
        stat.added += added.unwrap_or(0);
        stat.removed += removed.unwrap_or(0);
        stat.files.push(FileStat {
            path,
            added,
            removed,
            renamed_from,
        });
    }
    stat
}

/// `git diff base head` from immutable objects, truncated to `max_bytes` (char boundary).
pub fn diff_text(subject: &ChangeSubject, max_bytes: usize) -> Result<DiffText, SubjectError> {
    diff_text_path(subject, None, max_bytes)
}

/// [`diff_text`] limited to one path (literal pathspec) when `path` is given.
pub fn diff_text_path(
    subject: &ChangeSubject,
    path: Option<&str>,
    max_bytes: usize,
) -> Result<DiffText, SubjectError> {
    require_immutable(subject)?;
    let repo = Path::new(&subject.repo.root);
    let mut args = vec!["diff"];
    args.extend(DIFF_FLAGS);
    args.extend([subject.base_sha.as_str(), subject.content_sha()]);
    let spec = path.map(|p| format!(":(literal){p}"));
    if let Some(spec) = &spec {
        args.extend(["--", spec.as_str()]);
    }
    let out = gitcmd::run_bytes(repo, &args)?;
    let full = String::from_utf8_lossy(&out);
    let (t, truncated) = truncate_utf8(&full, max_bytes);
    Ok(DiffText {
        text: t.to_string(),
        truncated,
        total_bytes: out.len(),
    })
}

#[cfg(test)]
pub(crate) mod testrepo {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// A throwaway repository with isolated git config.
    pub struct TestRepo {
        pub dir: tempfile::TempDir,
    }

    impl TestRepo {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let r = TestRepo { dir };
            r.git(&["init", "-q", "-b", "main"]);
            r
        }
        pub fn path(&self) -> PathBuf {
            self.dir.path().canonicalize().unwrap()
        }
        pub fn git(&self, args: &[&str]) -> String {
            let out = Command::new("git")
                .arg("-C")
                .arg(self.dir.path())
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        pub fn write(&self, rel: &str, content: &str) {
            let p = self.dir.path().join(rel);
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }
        pub fn commit(&self, msg: &str) -> String {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", msg]);
            self.git(&["rev-parse", "HEAD"])
        }
        pub fn root(&self) -> &Path {
            self.dir.path()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testrepo::TestRepo;
    use super::*;

    #[test]
    fn subject_id_is_content_addressed() {
        let repo = RepoIdentity {
            root: "/r".into(),
            origin_url: None,
        };
        let a = ChangeSubject::new(
            repo.clone(),
            "b".into(),
            "h".into(),
            None,
            DirtyState::Clean,
            SubjectKind::Committed,
            1,
        );
        let b = ChangeSubject::new(
            repo.clone(),
            "b".into(),
            "h".into(),
            None,
            DirtyState::Clean,
            SubjectKind::Committed,
            999,
        );
        assert_eq!(a.id, b.id, "capture time is not identity");
        let c = ChangeSubject::new(
            repo,
            "b".into(),
            "h2".into(),
            None,
            DirtyState::Clean,
            SubjectKind::Committed,
            1,
        );
        assert_ne!(a.id, c.id);
        assert!(a.verify_id());
        let mut t = a.clone();
        t.head_sha = "x".into();
        assert!(!t.verify_id());
    }

    #[test]
    fn capture_committed_and_diffs_from_immutable_objects() {
        let r = TestRepo::new();
        r.write("a.txt", "one\n");
        let base = r.commit("base");
        r.write("a.txt", "one\ntwo\n");
        r.write("bin.dat", "\0\x01\x02");
        let head = r.commit("head");
        let s = capture_committed(r.root(), "HEAD~1", "HEAD").unwrap();
        assert_eq!(s.base_sha, base);
        assert_eq!(s.head_sha, head);
        assert_eq!(s.kind, SubjectKind::Committed);
        assert_eq!(s.repo.root, r.path().to_string_lossy());

        // Live edits must not leak into the immutable diff.
        r.write("a.txt", "live edit\n");
        let st = diff_stat(&s).unwrap();
        assert_eq!(st.added, 1);
        let a = st.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!((a.added, a.removed), (Some(1), Some(0)));
        let b = st.files.iter().find(|f| f.path == "bin.dat").unwrap();
        assert_eq!(b.added, None);
        let d = diff_text(&s, 1 << 20).unwrap();
        assert!(d.text.contains("+two"));
        assert!(!d.text.contains("live edit"));
        assert!(!d.truncated);
        let d = diff_text(&s, 10).unwrap();
        assert!(d.truncated && d.text.len() <= 10);
        // One path only.
        let d = diff_text_path(&s, Some("a.txt"), 1 << 20).unwrap();
        assert!(d.text.contains("+two") && !d.text.contains("bin.dat"));
        let d = diff_text_path(&s, Some("missing.txt"), 1 << 20).unwrap();
        assert!(d.text.is_empty());
        // The live file is untouched.
        assert_eq!(
            std::fs::read_to_string(r.root().join("a.txt")).unwrap(),
            "live edit\n"
        );
    }

    #[test]
    fn rename_numstat_parsed() {
        let r = TestRepo::new();
        r.write("old.txt", "a\nb\nc\nd\n");
        r.commit("1");
        r.git(&["mv", "old.txt", "new.txt"]);
        r.commit("2");
        let s = capture_committed(r.root(), "HEAD~1", "HEAD").unwrap();
        let st = diff_stat(&s).unwrap();
        assert_eq!(st.files.len(), 1);
        assert_eq!(st.files[0].path, "new.txt");
        assert_eq!(st.files[0].renamed_from.as_deref(), Some("old.txt"));
    }

    #[test]
    fn live_subject_refuses_immutable_diff() {
        let r = TestRepo::new();
        r.write("a", "1");
        let h = r.commit("1");
        let s = ChangeSubject::new(
            repo_identity(r.root()).unwrap(),
            h.clone(),
            h,
            Some("d".into()),
            DirtyState::Dirty,
            SubjectKind::CheckoutLive,
            0,
        );
        assert!(matches!(diff_stat(&s), Err(SubjectError::NotImmutable(_))));
    }

    #[test]
    fn rev_parse_and_ancestry() {
        let r = TestRepo::new();
        r.write("a", "1");
        let c1 = r.commit("1");
        r.write("a", "2");
        let c2 = r.commit("2");
        assert_eq!(rev_parse(r.root(), "HEAD").unwrap(), c2);
        assert!(is_ancestor(r.root(), &c1, &c2).unwrap());
        assert!(!is_ancestor(r.root(), &c2, &c1).unwrap());
        assert!(matches!(
            rev_parse(r.root(), "nope"),
            Err(SubjectError::BadRevision(_))
        ));
        assert!(rev_parse(r.root(), "--all").is_err());
    }

    #[test]
    fn baseline_digest_covers_staged_unstaged_untracked_without_touching_tree() {
        let r = TestRepo::new();
        r.write("a.txt", "base\n");
        r.commit("1");
        let clean = observation_baseline(r.root()).unwrap();
        assert_eq!(clean.dirty_state, DirtyState::Clean);
        assert!(!clean.may_include_preexisting_changes);
        assert!(clean.change_digest.is_some());

        r.write("new/untracked.txt", "u1");
        let b1 = observation_baseline(r.root()).unwrap();
        assert_eq!(b1.dirty_state, DirtyState::Dirty);
        assert!(b1.may_include_preexisting_changes);
        // Untracked content changes the digest even though status output is identical.
        r.write("new/untracked.txt", "u2");
        let b2 = observation_baseline(r.root()).unwrap();
        assert_ne!(b1.change_digest, b2.change_digest);
        // Staged vs unstaged of the same content differ.
        r.write("a.txt", "changed\n");
        let unstaged = observation_baseline(r.root()).unwrap();
        r.git(&["add", "a.txt"]);
        let staged = observation_baseline(r.root()).unwrap();
        assert_ne!(unstaged.change_digest, staged.change_digest);
        // Deterministic.
        assert_eq!(
            staged.change_digest,
            observation_baseline(r.root()).unwrap().change_digest
        );
        // Nothing was stashed/reset: staged change and untracked file still present.
        assert!(r.git(&["status", "--porcelain"]).contains("M  a.txt"));
        assert!(r.root().join("new/untracked.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn untracked_executable_bit_and_symlinks_change_the_digest() {
        use std::os::unix::fs::PermissionsExt;
        let r = TestRepo::new();
        r.write("a.txt", "base\n");
        r.commit("1");
        r.write("run.sh", "#!/bin/sh\n");
        let plain = observation_baseline(r.root()).unwrap();
        let p = r.root().join("run.sh");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let exec = observation_baseline(r.root()).unwrap();
        assert_ne!(
            plain.change_digest, exec.change_digest,
            "chmod +x is a change"
        );
        // Group/other execute bits alone are not recorded by Git, so they are not a change.
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            observation_baseline(r.root()).unwrap().change_digest,
            plain.change_digest
        );
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o655)).unwrap();
        assert_eq!(
            observation_baseline(r.root()).unwrap().change_digest,
            plain.change_digest
        );
        // A symlink with the same "content" as a file is a different entry.
        std::fs::remove_file(&p).unwrap();
        std::os::unix::fs::symlink("#!/bin/sh\n", &p).unwrap();
        assert_ne!(
            observation_baseline(r.root()).unwrap().change_digest,
            plain.change_digest
        );
    }

    #[test]
    fn submodule_status_records_are_parsed() {
        let status = b"1 .M S.M. 160000 160000 160000 aaa aaa sub dir\0\
            1 .M N... 100644 100644 100644 bbb bbb plain.txt\0\
            2 R. S... 160000 160000 160000 ccc ccc R100 new sub\0old sub\0\
            ? untracked\0";
        assert_eq!(
            submodule_paths(status),
            vec!["sub dir".to_string(), "new sub".to_string()]
        );
        assert_eq!(untracked_paths(status), vec!["untracked".to_string()]);
    }

    #[test]
    fn baseline_on_unborn_head() {
        let r = TestRepo::new();
        r.write("x", "1");
        let b = observation_baseline(r.root()).unwrap();
        assert!(b.head.is_none());
        assert_eq!(b.dirty_state, DirtyState::Dirty);
    }

    #[test]
    fn review_base_proposals() {
        let r = TestRepo::new();
        r.write("a", "1");
        let root_commit = r.commit("1");
        r.git(&["checkout", "-q", "-b", "feature"]);
        r.write("a", "2");
        let feat = r.commit("2");
        r.git(&["checkout", "-q", "main"]);
        r.write("b", "x");
        let main2 = r.commit("main2");
        r.git(&["checkout", "-q", "feature"]);

        let p = propose_review_base(r.root(), None, Some("main"), None).unwrap();
        assert_eq!(p.base_sha, root_commit);
        assert_eq!(
            p.reason,
            BaseReason::MergeBaseWithTarget {
                branch: "main".into()
            }
        );
        let p = propose_review_base(r.root(), None, None, Some("main")).unwrap();
        assert!(matches!(p.reason, BaseReason::MergeBaseWithDefault { .. }));
        let p = propose_review_base(r.root(), Some(&main2), Some("main"), None).unwrap();
        assert_eq!(p.base_sha, main2);
        assert!(matches!(p.reason, BaseReason::OwnedTaskBase { .. }));
        // Unresolvable target, no default → HEAD with warnings.
        let p = propose_review_base(r.root(), None, Some("origin/nope"), None).unwrap();
        assert_eq!(p.base_sha, feat);
        assert_eq!(p.reason, BaseReason::HeadFallback);
        assert!(p.warnings.iter().any(|w| w.contains("omitted")));
        assert!(p.warnings.iter().any(|w| w.contains("origin/nope")));

        // Default branch: local main/master, or origin/HEAD's target.
        assert_eq!(current_branch(r.root()).as_deref(), Some("feature"));
        assert_eq!(default_branch(r.root()).as_deref(), Some("main"));
        r.git(&["branch", "-q", "-m", "main", "trunk"]);
        assert_eq!(default_branch(r.root()), None);
        r.git(&["update-ref", "refs/remotes/origin/trunk", &main2]);
        r.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ]);
        assert_eq!(default_branch(r.root()).as_deref(), Some("trunk"));
        r.git(&["branch", "-q", "-D", "trunk"]);
        assert_eq!(default_branch(r.root()).as_deref(), Some("origin/trunk"));
    }

    #[test]
    fn rebase_changes_subject_identity() {
        let r = TestRepo::new();
        r.write("a", "1");
        r.commit("1");
        r.git(&["checkout", "-q", "-b", "feature"]);
        r.write("f", "feature");
        r.commit("feat");
        let before = capture_committed(r.root(), "main", "HEAD").unwrap();
        r.git(&["checkout", "-q", "main"]);
        r.write("m", "main");
        r.commit("main");
        r.git(&["checkout", "-q", "feature"]);
        r.git(&["rebase", "-q", "main"]);
        let after = capture_committed(r.root(), "main", "HEAD").unwrap();
        assert_ne!(before.id, after.id);
        assert_ne!(before.head_sha, after.head_sha);
    }
}
