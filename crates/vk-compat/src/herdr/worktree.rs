//! Herdr 0.9.3's git worktree helpers (`src/worktree.rs`) for `worktree.list`, `worktree.open`,
//! `worktree.create` and `worktree.remove`: the porcelain parser, the generated branch name, the
//! checkout directory slug and the `git` arguments with `trust_repository`.

use std::path::{Path, PathBuf};

const DEFAULT_WORKTREE_PREFIX: &str = "worktree";

/// One entry of `git worktree list --porcelain`, as Herdr reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingWorktree {
    pub path: PathBuf,
    /// Short branch name (`refs/heads/` removed).
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_prunable: bool,
}

/// Herdr's `parse_worktree_list_porcelain`.
pub fn parse_porcelain(output: &str) -> Vec<ExistingWorktree> {
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    let (mut bare, mut detached, mut prunable) = (false, false, false);
    let mut finish = |path: &mut Option<PathBuf>,
                      branch: &mut Option<String>,
                      bare: &mut bool,
                      detached: &mut bool,
                      prunable: &mut bool| {
        if let Some(p) = path.take() {
            entries.push(ExistingWorktree {
                path: p,
                branch: branch.take(),
                is_bare: *bare,
                is_detached: *detached,
                is_prunable: *prunable,
            });
        }
        *bare = false;
        *detached = false;
        *prunable = false;
    };
    for line in output.lines() {
        if line.trim().is_empty() {
            finish(
                &mut path,
                &mut branch,
                &mut bare,
                &mut detached,
                &mut prunable,
            );
            continue;
        }
        if let Some(v) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(v));
        } else if let Some(v) = line.strip_prefix("branch ") {
            branch = Some(v.strip_prefix("refs/heads/").unwrap_or(v).to_string());
        } else if line == "detached" {
            detached = true;
        } else if line == "bare" {
            bare = true;
        } else if line.starts_with("prunable") {
            prunable = true;
        }
    }
    finish(
        &mut path,
        &mut branch,
        &mut bare,
        &mut detached,
        &mut prunable,
    );
    entries
}

/// Herdr's `generated_branch_slug`: the branch `worktree.create` uses when the request names
/// none (`worktree/<adjective>-<noun>-<hex>`), from a seed (Herdr: microseconds since the
/// epoch).
pub fn generated_branch(seed: u64) -> String {
    let adjectives = [
        "brave", "calm", "clear", "green", "lucky", "quiet", "rapid", "silver",
    ];
    let nouns = [
        "river", "cloud", "field", "forest", "harbor", "meadow", "stone", "valley",
    ];
    let adjective = adjectives[(seed as usize) % adjectives.len()];
    let noun = nouns[((seed / adjectives.len() as u64) as usize) % nouns.len()];
    let suffix = seed & 0xffff;
    format!("{DEFAULT_WORKTREE_PREFIX}/{adjective}-{noun}-{suffix:04x}")
}

/// Herdr's `branch_to_path_slug`: the checkout directory name for a branch.
pub fn branch_slug(branch: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for ch in branch.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        DEFAULT_WORKTREE_PREFIX.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Herdr's `repository_git_args`: `-c safe.directory=<repo>` when the caller trusts the
/// repository, then `-C <repo>`.
pub fn git_args(repo: &Path, trust_repository: bool) -> Vec<String> {
    let mut args = Vec::new();
    if trust_repository {
        args.push("-c".to_string());
        args.push(format!("safe.directory={}", repo.display()));
    }
    args.push("-C".to_string());
    args.push(repo.display().to_string());
    args
}

/// The `git worktree add` arguments Herdr runs (after [`git_args`]): an existing local branch
/// is checked out, otherwise a new branch is created from `base`.
pub fn add_args(path: &Path, branch: &str, base: &str, branch_exists: bool) -> Vec<String> {
    let mut args = vec!["worktree".to_string(), "add".to_string()];
    if branch_exists {
        args.push(path.display().to_string());
        args.push(branch.to_string());
    } else {
        args.push("-b".to_string());
        args.push(branch.to_string());
        args.push(path.display().to_string());
        args.push(base.to_string());
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain() {
        let out = "worktree /r\nHEAD 1\nbranch refs/heads/main\n\nworktree /r-wt\nHEAD 2\ndetached\n\nworktree /gone\nHEAD 3\nbranch refs/heads/x\nprunable gitdir file points to non-existent location\n\nworktree /bare.git\nbare\n";
        let e = parse_porcelain(out);
        assert_eq!(e.len(), 4);
        assert_eq!(e[0].branch.as_deref(), Some("main"));
        assert!(!e[0].is_detached && !e[0].is_bare);
        assert!(e[1].is_detached && e[1].branch.is_none());
        assert!(e[2].is_prunable);
        assert!(e[3].is_bare && !e[3].is_prunable);
    }

    #[test]
    fn names() {
        assert_eq!(generated_branch(0), "worktree/brave-river-0000");
        assert_eq!(generated_branch(9), "worktree/calm-cloud-0009");
        assert_eq!(branch_slug("Feature/Big_Thing"), "feature-big-thing");
        assert_eq!(branch_slug("///"), "worktree");
    }

    #[test]
    fn git_arguments() {
        let r = Path::new("/r");
        assert_eq!(git_args(r, false), ["-C", "/r"]);
        assert_eq!(git_args(r, true), ["-c", "safe.directory=/r", "-C", "/r"]);
        assert_eq!(
            add_args(Path::new("/w"), "b", "HEAD", false),
            ["worktree", "add", "-b", "b", "/w", "HEAD"]
        );
        assert_eq!(
            add_args(Path::new("/w"), "b", "HEAD", true),
            ["worktree", "add", "/w", "b"]
        );
    }
}
