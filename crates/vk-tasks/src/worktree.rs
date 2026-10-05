//! Worktree creation, listing and lookup (05 §4).

use crate::git::{git, git_timeout, ref_exists, same_path};
use crate::naming::{DEFAULT_SLUG_MAX, render_branch, slugify, user_handle};
use crate::repo::{RepoInfo, repo_root};
use crate::{Error, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where task worktrees live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeRoot {
    /// `<dir>/<repo-name>/<slug>`
    Dir(PathBuf),
    /// `<repo parent>/<repo-name>-<slug>`
    Sibling,
}

impl WorktreeRoot {
    /// Parse the `tasks.root` config value: `"sibling"` or a directory
    /// (a leading `~` is expanded with `$HOME`).
    pub fn parse(s: &str) -> Result<Self> {
        if s.eq_ignore_ascii_case("sibling") {
            return Ok(Self::Sibling);
        }
        if s.is_empty() {
            return Err(Error::Config("tasks.root is empty".into()));
        }
        let expanded = if s == "~" || s.starts_with("~/") {
            let home = std::env::var_os("HOME")
                .ok_or_else(|| Error::Config("HOME is not set, cannot expand ~".into()))?;
            PathBuf::from(home).join(s.trim_start_matches('~').trim_start_matches('/'))
        } else {
            PathBuf::from(s)
        };
        Ok(Self::Dir(expanded))
    }
}

/// `[tasks]` keys that matter for checkout creation.
#[derive(Debug, Clone)]
pub struct WorktreeConfig {
    pub root: WorktreeRoot,
    pub branch_template: String,
    pub fetch_before_create: bool,
    pub fetch_timeout: Duration,
    pub slug_max_len: usize,
    /// Run `git submodule update --init --recursive` if `.gitmodules` exists.
    pub submodules: bool,
    /// Override for `{user}`; otherwise `git config user.name` / `$USER`.
    pub user: Option<String>,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            root: WorktreeRoot::parse("~/.vibeke/worktrees")
                .unwrap_or_else(|_| WorktreeRoot::Dir(PathBuf::from(".vibeke/worktrees"))),
            branch_template: "{user}/{slug}".into(),
            fetch_before_create: true,
            fetch_timeout: Duration::from_secs(5),
            slug_max_len: DEFAULT_SLUG_MAX,
            submodules: true,
            user: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CreateRequest {
    /// Any path inside the source repo (or one of its worktrees).
    pub repo: PathBuf,
    pub title: String,
    /// Explicit branch (used as-is). Existing branch is checked out, if free.
    pub branch: Option<String>,
    /// Base ref for a new branch. Default: `origin/<default>`, else the local
    /// default branch, else `HEAD`.
    pub base: Option<String>,
    /// Use this slug instead of deriving one from `title`.
    pub slug: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    Skipped,
    Fetched,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Checkout {
    pub path: PathBuf,
    pub branch: Option<String>,
    /// Base ref the branch was created from (None when an existing branch was checked out).
    pub base_ref: Option<String>,
    pub slug: String,
    /// Main repo root.
    pub repo_root: PathBuf,
    pub created_branch: bool,
    pub fetch: FetchOutcome,
    /// Non-fatal problems (e.g. submodule init failed).
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    pub head: Option<String>,
    /// Short branch name; None if detached or bare.
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub prunable: bool,
    pub prune_reason: Option<String>,
    /// First entry of the list: the main working tree.
    pub is_main: bool,
}

/// `git worktree list --porcelain`, parsed.
pub fn list_worktrees(repo: &Path) -> Result<Vec<WorktreeEntry>> {
    let out = git(repo, &["worktree", "list", "--porcelain"])?;
    Ok(parse_worktree_list(&out))
}

pub(crate) fn parse_worktree_list(out: &str) -> Vec<WorktreeEntry> {
    let mut list = Vec::new();
    for block in out.split("\n\n") {
        let mut e = WorktreeEntry {
            path: PathBuf::new(),
            head: None,
            branch: None,
            bare: false,
            detached: false,
            locked: false,
            lock_reason: None,
            prunable: false,
            prune_reason: None,
            is_main: false,
        };
        let mut any = false;
        for line in block.lines() {
            let (k, v) = line.split_once(' ').unwrap_or((line, ""));
            match k {
                "worktree" => {
                    e.path = PathBuf::from(v);
                    any = true;
                }
                "HEAD" => e.head = Some(v.to_string()),
                "branch" => e.branch = Some(v.strip_prefix("refs/heads/").unwrap_or(v).to_string()),
                "bare" => e.bare = true,
                "detached" => e.detached = true,
                "locked" => {
                    e.locked = true;
                    e.lock_reason = (!v.is_empty()).then(|| v.to_string());
                }
                "prunable" => {
                    e.prunable = true;
                    e.prune_reason = (!v.is_empty()).then(|| v.to_string());
                }
                _ => {}
            }
        }
        if any {
            e.is_main = list.is_empty();
            list.push(e);
        }
    }
    list
}

/// Find the worktree whose path is `path`.
pub fn find_worktree(repo: &Path, path: &Path) -> Result<WorktreeEntry> {
    list_worktrees(repo)?
        .into_iter()
        .find(|w| same_path(&w.path, path))
        .ok_or_else(|| Error::WorktreeNotFound(path.display().to_string()))
}

/// Find the worktree that has `branch` checked out.
pub fn find_worktree_by_branch(repo: &Path, branch: &str) -> Result<WorktreeEntry> {
    list_worktrees(repo)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(branch))
        .ok_or_else(|| Error::WorktreeNotFound(branch.to_string()))
}

/// Open an existing worktree as a [`Checkout`] (no changes made).
pub fn open_worktree(repo: &Path, path: &Path) -> Result<Checkout> {
    let info = repo_root(repo).ok_or_else(|| Error::NotARepo(repo.to_path_buf()))?;
    let w = find_worktree(&info.root, path)?;
    if w.prunable || !w.path.exists() {
        return Err(Error::WorktreeNotFound(format!(
            "{} (directory is missing)",
            path.display()
        )));
    }
    let slug = w
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Checkout {
        path: w.path.canonicalize().unwrap_or(w.path),
        branch: w.branch,
        base_ref: None,
        slug,
        repo_root: info.root,
        created_branch: false,
        fetch: FetchOutcome::Skipped,
        warnings: vec![],
    })
}

/// Default base for new branches.
pub fn default_base(info: &RepoInfo) -> String {
    if let Some(d) = &info.default_branch {
        if ref_exists(&info.root, &format!("refs/remotes/origin/{d}")) {
            return format!("origin/{d}");
        }
        if ref_exists(&info.root, &format!("refs/heads/{d}")) {
            return d.clone();
        }
    }
    "HEAD".into()
}

/// Where a worktree for `slug` would live.
pub fn worktree_path(root: &WorktreeRoot, repo_root: &Path, slug: &str) -> PathBuf {
    let name = repo_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    match root {
        WorktreeRoot::Dir(d) => d.join(&name).join(slug),
        WorktreeRoot::Sibling => repo_root
            .parent()
            .unwrap_or(repo_root)
            .join(format!("{name}-{slug}")),
    }
}

/// Create a git worktree for a task (steps 1-3 of the lifecycle).
pub fn create_worktree(req: &CreateRequest, cfg: &WorktreeConfig) -> Result<Checkout> {
    let info = repo_root(&req.repo).ok_or_else(|| Error::NotARepo(req.repo.clone()))?;
    let main = info.root.clone();
    let mut warnings = Vec::new();

    let fetch = if cfg.fetch_before_create && info.remote_url.is_some() {
        match git_timeout(
            &main,
            &["fetch", "--quiet", "origin"],
            Some(cfg.fetch_timeout),
        ) {
            Ok(_) => FetchOutcome::Fetched,
            Err(e) => FetchOutcome::Failed(e.to_string()),
        }
    } else {
        FetchOutcome::Skipped
    };

    let base = req.base.clone().unwrap_or_else(|| default_base(&info));
    let slug_base = req
        .slug
        .clone()
        .unwrap_or_else(|| slugify(&req.title, cfg.slug_max_len));
    let user = cfg.user.clone().unwrap_or_else(|| user_handle(&main));
    let existing = list_worktrees(&main)?;

    // Pick a free slug (and branch name, unless explicit).
    let mut chosen = None;
    for n in 1u32..10_000 {
        let slug = if n == 1 {
            slug_base.clone()
        } else {
            format!("{slug_base}-{n}")
        };
        let path = worktree_path(&cfg.root, &main, &slug);
        if path.exists() {
            continue;
        }
        let branch = match &req.branch {
            Some(b) => b.clone(),
            None => {
                let b = render_branch(&cfg.branch_template, &user, &slug);
                if ref_exists(&main, &format!("refs/heads/{b}")) {
                    continue;
                }
                b
            }
        };
        chosen = Some((slug, path, branch));
        break;
    }
    let (slug, path, branch) =
        chosen.ok_or_else(|| Error::Config("could not find a free slug".into()))?;

    let branch_exists = ref_exists(&main, &format!("refs/heads/{branch}"));
    if branch_exists
        && let Some(w) = existing
            .iter()
            .find(|w| w.branch.as_deref() == Some(&branch))
    {
        return Err(Error::BranchInUse {
            branch,
            path: w.path.clone(),
        });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let path_s = path.to_string_lossy().into_owned();
    if branch_exists {
        git(&main, &["worktree", "add", &path_s, &branch])?;
    } else {
        // --no-track: a task branch must not silently track (and push to) the base.
        git(
            &main,
            &[
                "worktree",
                "add",
                "--no-track",
                "-b",
                &branch,
                &path_s,
                &base,
            ],
        )?;
    }

    if cfg.submodules
        && path.join(".gitmodules").exists()
        && let Err(e) = git(&path, &["submodule", "update", "--init", "--recursive"])
    {
        warnings.push(format!("submodule init failed: {e}"));
    }

    Ok(Checkout {
        path: path.canonicalize().unwrap_or(path),
        branch: Some(branch),
        base_ref: (!branch_exists).then_some(base),
        slug,
        repo_root: main,
        created_branch: !branch_exists,
        fetch,
        warnings,
    })
}

/// Recreate the worktree of an archived task from its (kept) branch.
pub fn restore_worktree(
    repo: &Path,
    branch: &str,
    slug: &str,
    cfg: &WorktreeConfig,
) -> Result<Checkout> {
    create_worktree(
        &CreateRequest {
            repo: repo.to_path_buf(),
            title: slug.to_string(),
            branch: Some(branch.to_string()),
            base: None,
            slug: Some(slug.to_string()),
        },
        cfg,
    )
}
