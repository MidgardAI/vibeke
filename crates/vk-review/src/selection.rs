//! Selected-patch snapshots (15 §5, T4): a user-selected part of the uncommitted work — whole
//! files or a unified diff — applied on top of the capture-time HEAD and stored as an immutable
//! snapshot commit under `refs/vibeke/snapshots/`, exactly like a dirty snapshot.
//!
//! The selection defines the **review scope**. A check run against a selected-patch subject
//! runs in a disposable checkout of that snapshot commit, so it verifies the selection alone,
//! isolated from the rest of the checkout ("an isolated verification snapshot can establish
//! that separately"). Checks run against the whole checkout keep their whole-checkout subject
//! and are never presented as proof that the selection passes on its own. Accepting a selected
//! patch never claims the moving checkout is reviewed.
//!
//! Capture is validated like a dirty snapshot: HEAD and the whole checkout's change digest must
//! agree before and after writing the selection's tree (whole files: the tree must also be the
//! same when written twice), otherwise **Workspace changing — verification subject
//! unavailable**. The user's index, files and branches are never touched; everything goes
//! through a private index file.

use std::path::{Component, Path, PathBuf};

use crate::gitcmd;
use crate::snapshot::{REF_PREFIX, SnapshotError, SnapshotOptions, TempIndex, write_trees};
use crate::subject::{self, ChangeSubject, DirtyState, Selection, SelectionMode, SnapshotRef};
use crate::{new_id, now_ms};

/// What the user selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchSelection {
    /// Whole files (relative paths inside the repository); deletions included.
    Paths(Vec<String>),
    /// A unified diff (`git diff` format, binary hunks allowed) applied to HEAD.
    Patch(String),
}

impl PatchSelection {
    pub fn mode(&self) -> SelectionMode {
        match self {
            PatchSelection::Paths(_) => SelectionMode::Paths,
            PatchSelection::Patch(_) => SelectionMode::Patch,
        }
    }
}

/// A plain relative path inside the repository (no `..`, no root, no empty path).
fn valid_rel(p: &str) -> bool {
    let path = Path::new(p);
    !p.is_empty()
        && !path.is_absolute()
        && path.components().all(|c| matches!(c, Component::Normal(_)))
}

/// Files a unified diff touches (both sides of each `diff --git a/x b/y` header).
pub fn patch_paths(patch: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ")
            && let Some((a, b)) = rest.split_once(" b/")
        {
            for p in [a.strip_prefix("a/").unwrap_or(a), b] {
                if !out.iter().any(|x| x == p) {
                    out.push(p.to_string());
                }
            }
        }
    }
    out
}

/// The tree of `head` plus only the selection, written through a private index. Blocking.
pub fn selection_tree(
    repo: &Path,
    head: &str,
    sel: &PatchSelection,
) -> Result<String, SnapshotError> {
    let git_dir = PathBuf::from(gitcmd::run(repo, &["rev-parse", "--absolute-git-dir"])?);
    let tmp = TempIndex(git_dir.join(format!("vibeke-select-{}.index", new_id())));
    let tmp_s = tmp.0.to_string_lossy().into_owned();
    let env = [
        ("GIT_INDEX_FILE", tmp_s.as_str()),
        ("GIT_LITERAL_PATHSPECS", "1"),
    ];
    gitcmd::run_env(repo, &["read-tree", head], &env)?;
    match sel {
        PatchSelection::Paths(paths) => {
            if paths.is_empty() {
                return Err(SnapshotError::InvalidSelection("no paths selected".into()));
            }
            if let Some(bad) = paths.iter().find(|p| !valid_rel(p)) {
                return Err(SnapshotError::InvalidSelection(format!(
                    "`{bad}` is not a relative path inside the repository"
                )));
            }
            let mut args = vec!["add", "-A", "--"];
            args.extend(paths.iter().map(String::as_str));
            gitcmd::run_env(repo, &args, &env)?;
        }
        PatchSelection::Patch(text) => {
            if text.trim().is_empty() {
                return Err(SnapshotError::InvalidSelection("empty patch".into()));
            }
            let touched = patch_paths(text);
            if touched.is_empty() {
                return Err(SnapshotError::InvalidSelection(
                    "not a unified diff (no `diff --git` header)".into(),
                ));
            }
            if let Some(bad) = touched.iter().find(|p| !valid_rel(p)) {
                return Err(SnapshotError::InvalidSelection(format!(
                    "the patch touches `{bad}`, outside the repository"
                )));
            }
            // A private file, removed on drop like the index copy.
            let file = TempIndex(git_dir.join(format!("vibeke-select-{}.patch", new_id())));
            std::fs::write(&file.0, text.as_bytes())
                .map_err(|e| SnapshotError::Io(format!("patch file: {e}")))?;
            let f = file.0.to_string_lossy().into_owned();
            let out = gitcmd::run_raw_env(
                repo,
                &["apply", "--cached", "--binary", "--whitespace=nowarn", &f],
                &env,
                gitcmd::GIT_TIMEOUT,
            )?;
            if out.code != Some(0) {
                return Err(SnapshotError::InvalidSelection(format!(
                    "the patch does not apply to HEAD: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
        }
    }
    Ok(gitcmd::run_env(repo, &["write-tree"], &env)?)
}

/// Capture a validated selected-patch snapshot of the checkout containing `repo` against
/// review base `base_sha`. Blocking; run it off the state actor / render path.
pub fn capture_selected_patch(
    repo: &Path,
    base_sha: &str,
    sel: &PatchSelection,
    opts: &SnapshotOptions,
) -> Result<ChangeSubject, SnapshotError> {
    let identity = subject::repo_identity(repo)?;
    let root = PathBuf::from(&identity.root);
    let base = subject::rev_parse(&root, base_sha)?;
    let attempts = opts.max_attempts.max(1);
    let selected_paths = match sel {
        PatchSelection::Paths(p) => p.clone(),
        PatchSelection::Patch(t) => patch_paths(t),
    };
    for attempt in 1..=attempts {
        if attempt > 1 {
            std::thread::sleep(opts.retry_delay);
        }
        let before = subject::observation_baseline(&root)?;
        let Some(head) = before.head.clone() else {
            return Err(SnapshotError::UnbornHead);
        };
        match before.dirty_state {
            DirtyState::Clean => return Err(SnapshotError::Clean),
            DirtyState::Unknown => continue,
            DirtyState::Dirty => {}
        }
        // A changed submodule is only a problem when it is part of the selection.
        let sub_hit: Vec<String> = before
            .changed_submodules
            .iter()
            .filter(|s| {
                selected_paths
                    .iter()
                    .any(|p| p == *s || p.starts_with(&format!("{s}/")))
            })
            .cloned()
            .collect();
        if !sub_hit.is_empty() {
            return Err(SnapshotError::UnsupportedCapture {
                what: "submodule",
                paths: sub_hit,
            });
        }
        let tree = match selection_tree(&root, &head, sel) {
            Ok(t) => t,
            Err(e @ SnapshotError::InvalidSelection(_)) => return Err(e),
            // A file changing under `git add` fails it: a concurrent writer, retry.
            Err(SnapshotError::Git(_)) if attempt < attempts => continue,
            Err(e) => return Err(e),
        };
        let head_tree = gitcmd::run(&root, &["rev-parse", &format!("{head}^{{tree}}")])?;
        if tree == head_tree {
            return Err(SnapshotError::NothingSelected(
                "the selected files hold no uncommitted change".into(),
            ));
        }
        let again = match sel {
            PatchSelection::Paths(_) => selection_tree(&root, &head, sel).ok(),
            PatchSelection::Patch(_) => Some(tree.clone()),
        };
        let after = subject::observation_baseline(&root)?;
        let consistent = after.head.as_deref() == Some(head.as_str())
            && after.change_digest.is_some()
            && after.change_digest == before.change_digest
            && again.as_deref() == Some(tree.as_str());
        if !consistent {
            continue;
        }
        // Whether uncommitted work outside the selection exists (shown as a limitation).
        let excludes_other_changes = write_trees(&root)
            .map(|(_, full)| full != tree)
            .unwrap_or(true);
        let digest = before.change_digest.expect("dirty state has a digest");
        let patch_digest = match sel {
            PatchSelection::Patch(t) => Some(blake3::hash(t.as_bytes()).to_hex().to_string()),
            PatchSelection::Paths(_) => None,
        };
        let mut paths = selected_paths.clone();
        paths.sort();
        paths.dedup();
        let commit = commit_selected(&root, &tree, &head, &base, sel.mode(), &paths)?;
        let ref_name = format!("{REF_PREFIX}{commit}");
        gitcmd::run(&root, &["update-ref", &ref_name, &commit])?;
        return Ok(ChangeSubject::selected_patch(
            identity,
            base,
            head,
            digest,
            SnapshotRef {
                commit,
                tree,
                staged_tree: None,
                ref_name,
                attempts: attempt,
            },
            Selection {
                mode: sel.mode(),
                paths,
                patch_digest,
                excludes_other_changes,
            },
            now_ms(),
        ));
    }
    Err(SnapshotError::WorkspaceChanging { attempts })
}

/// Whether a selected-patch subject still describes the checkout: the same HEAD, and for whole
/// files the selected paths' working-tree content gives the same tree; for a patch (whose
/// hunks can't be compared against partially selected files) the whole checkout's change
/// digest is unchanged. Blocking.
pub fn selection_is_current(repo: &Path, s: &ChangeSubject) -> bool {
    let (Some(sel), Some(sn)) = (&s.selection, &s.snapshot) else {
        return false;
    };
    if subject::rev_parse(repo, "HEAD").ok().as_deref() != Some(s.head_sha.as_str()) {
        return false;
    }
    match sel.mode {
        SelectionMode::Paths => {
            selection_tree(repo, &s.head_sha, &PatchSelection::Paths(sel.paths.clone()))
                .is_ok_and(|t| t == sn.tree)
        }
        SelectionMode::Patch => subject::observation_baseline(repo)
            .ok()
            .and_then(|b| b.change_digest)
            .is_some_and(|d| Some(d) == s.dirty_digest),
    }
}

/// Deterministic commit (fixed identity and time): the same selection on the same HEAD gives
/// the same commit and subject id.
fn commit_selected(
    root: &Path,
    tree: &str,
    head: &str,
    base: &str,
    mode: SelectionMode,
    paths: &[String],
) -> Result<String, SnapshotError> {
    let msg = format!(
        "Vibeke selected-patch snapshot\n\nVibeke-Base: {base}\nVibeke-Head: {head}\nVibeke-Selection: {}\n{}",
        match mode {
            SelectionMode::Paths => "paths",
            SelectionMode::Patch => "patch",
        },
        paths
            .iter()
            .map(|p| format!("Vibeke-Path: {p}\n"))
            .collect::<String>()
    );
    let env = [
        ("GIT_AUTHOR_NAME", "Vibeke"),
        ("GIT_AUTHOR_EMAIL", "vibeke@localhost"),
        ("GIT_AUTHOR_DATE", "946684800 +0000"),
        ("GIT_COMMITTER_NAME", "Vibeke"),
        ("GIT_COMMITTER_EMAIL", "vibeke@localhost"),
        ("GIT_COMMITTER_DATE", "946684800 +0000"),
    ];
    Ok(gitcmd::run_env(
        root,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            tree,
            "-p",
            head,
            "-m",
            &msg,
        ],
        &env,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subject::SubjectKind;
    use crate::subject::testrepo::TestRepo;
    use std::time::Duration;

    fn fast() -> SnapshotOptions {
        SnapshotOptions {
            max_attempts: 3,
            retry_delay: Duration::from_millis(5),
        }
    }

    fn show(r: &TestRepo, commit: &str, path: &str) -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(r.root())
            .args(["show", &format!("{commit}:{path}")])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    #[test]
    fn whole_file_selection_holds_only_the_selected_files_and_leaves_the_checkout_alone() {
        let r = TestRepo::new();
        r.write("a.txt", "a\n");
        r.write("b.txt", "b\n");
        let base = r.commit("base");
        r.write("a.txt", "a changed\n");
        r.write("b.txt", "b changed\n");
        r.write("new.txt", "new\n");
        let status_before = r.git(&["status", "--porcelain=v2", "-z", "--untracked-files=all"]);
        let index_before = std::fs::read(r.root().join(".git/index")).unwrap();

        let sel = PatchSelection::Paths(vec!["a.txt".into(), "new.txt".into()]);
        let s = capture_selected_patch(r.root(), &base, &sel, &fast()).unwrap();
        assert_eq!(s.kind, SubjectKind::SelectedPatch);
        assert!(s.is_immutable() && s.verify_id());
        let sn = s.snapshot.clone().unwrap();
        let selection = s.selection.clone().unwrap();
        assert_eq!(selection.paths, vec!["a.txt", "new.txt"]);
        assert!(selection.excludes_other_changes, "b.txt stays outside");
        assert_eq!(
            show(&r, &sn.commit, "a.txt").as_deref(),
            Some("a changed\n")
        );
        assert_eq!(show(&r, &sn.commit, "new.txt").as_deref(), Some("new\n"));
        assert_eq!(show(&r, &sn.commit, "b.txt").as_deref(), Some("b\n"));
        // The user's checkout is untouched.
        assert_eq!(
            r.git(&["status", "--porcelain=v2", "-z", "--untracked-files=all"]),
            status_before
        );
        assert_eq!(
            std::fs::read(r.root().join(".git/index")).unwrap(),
            index_before
        );
        // Current until a selected file changes; an unselected edit doesn't matter.
        assert!(selection_is_current(r.root(), &s));
        r.write("b.txt", "b changed again\n");
        assert!(selection_is_current(r.root(), &s));
        r.write("a.txt", "a changed again\n");
        assert!(!selection_is_current(r.root(), &s));
    }

    #[test]
    fn identical_selection_gives_the_identical_subject() {
        let r = TestRepo::new();
        r.write("a.txt", "a\n");
        let base = r.commit("base");
        r.write("a.txt", "x\n");
        let sel = PatchSelection::Paths(vec!["a.txt".into()]);
        let one = capture_selected_patch(r.root(), &base, &sel, &fast()).unwrap();
        let two = capture_selected_patch(r.root(), &base, &sel, &fast()).unwrap();
        assert_eq!(one.id, two.id);
        // A dirty snapshot of the same content is a different subject (different kind).
        let full = crate::snapshot::capture_dirty_snapshot(r.root(), &base, &fast()).unwrap();
        assert_ne!(full.id, one.id);
    }

    #[test]
    fn patch_selection_applies_hunks_to_head() {
        let r = TestRepo::new();
        r.write("a.txt", "one\ntwo\nthree\n");
        let base = r.commit("base");
        r.write("a.txt", "ONE\ntwo\nthree\n");
        r.write("b.txt", "unrelated\n");
        let patch = r.git(&["diff", "--", "a.txt"]);
        let s = capture_selected_patch(
            r.root(),
            &base,
            &PatchSelection::Patch(format!("{patch}\n")),
            &fast(),
        )
        .unwrap();
        let sn = s.snapshot.clone().unwrap();
        assert_eq!(
            show(&r, &sn.commit, "a.txt").as_deref(),
            Some("ONE\ntwo\nthree\n")
        );
        assert_eq!(show(&r, &sn.commit, "b.txt"), None);
        let sel = s.selection.unwrap();
        assert_eq!(sel.mode, SelectionMode::Patch);
        assert!(sel.patch_digest.is_some());
        assert_eq!(sel.paths, vec!["a.txt"]);
    }

    #[test]
    fn invalid_or_empty_selections_are_refused() {
        let r = TestRepo::new();
        r.write("a.txt", "a\n");
        r.write("b.txt", "b\n");
        let base = r.commit("base");
        r.write("a.txt", "dirty\n");
        let e = capture_selected_patch(
            r.root(),
            &base,
            &PatchSelection::Paths(vec!["../etc/passwd".into()]),
            &fast(),
        )
        .unwrap_err();
        assert_eq!(e.reason(), "invalid_selection");
        let e = capture_selected_patch(
            r.root(),
            &base,
            &PatchSelection::Paths(vec!["b.txt".into()]),
            &fast(),
        )
        .unwrap_err();
        assert_eq!(e.reason(), "nothing_selected");
        let e = capture_selected_patch(
            r.root(),
            &base,
            &PatchSelection::Patch(
                "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-nope\n+x\n"
                    .into(),
            ),
            &fast(),
        )
        .unwrap_err();
        assert_eq!(e.reason(), "invalid_selection");
    }

    #[test]
    fn patch_paths_reads_both_sides() {
        let p = "diff --git a/x.rs b/y.rs\nsimilarity index 90%\ndiff --git a/z b/z\n";
        assert_eq!(patch_paths(p), vec!["x.rs", "y.rs", "z"]);
    }
}
