//! jj (Jujutsu) workspaces as task checkouts (05 §4 `jj` backend, M4).
//!
//! * Create: `jj workspace add --name <slug> -r <base> <path>` from the repo root. The default
//!   base is `trunk()` when it resolves to a real commit, else `@-`.
//! * The task's branch name becomes a jj **bookmark**, created when work is committed
//!   ([`Jj::bookmark_set`]); co-located git repos (`.jj` + `.git`) keep working with git tools.
//! * Status: `jj log -r '@ | @-'` with a fixed template (change/commit ids, bookmarks, empty,
//!   conflict, description).
//! * Remove: `jj workspace forget <name>` then delete the directory. Forgetting never loses work:
//!   the workspace's commits stay in the repo (visible in `jj log`), so there is no dirty check.
//!
//! The `jj` binary is `$VIBEKE_JJ` or `jj` on `PATH`; tests point it at a fake.

use crate::naming::{render_branch, slugify, user_handle};
use crate::worktree::{Checkout, CreateRequest, FetchOutcome, WorktreeConfig, worktree_path};
use crate::{Error, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct Jj {
    pub bin: PathBuf,
}

impl Default for Jj {
    fn default() -> Self {
        Jj {
            bin: std::env::var_os("VIBEKE_JJ")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("jj")),
        }
    }
}

/// `jj log` facts about a workspace's working-copy commit and its parent.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JjStatus {
    pub change_id: String,
    pub commit_id: String,
    /// Bookmarks on `@` or `@-` (the usual place after `jj commit`).
    pub bookmarks: Vec<String>,
    /// `@` has no changes.
    pub empty: bool,
    pub conflict: bool,
    pub description: String,
    pub parent_change_id: Option<String>,
}

/// One entry of `jj workspace list`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct JjWorkspace {
    pub name: String,
    pub summary: String,
}

/// The jj repo containing `path`: nearest ancestor with a `.jj` directory.
pub fn find_root(path: &Path) -> Option<PathBuf> {
    let start = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut cur = Some(start.as_path());
    while let Some(d) = cur {
        if d.join(".jj").is_dir() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

/// Co-located repo (`.jj` and `.git` side by side): git tools keep working.
pub fn is_colocated(root: &Path) -> bool {
    root.join(".git").exists()
}

const LOG_TEMPLATE: &str = r#"change_id.short() ++ "\t" ++ commit_id.short() ++ "\t" ++ bookmarks.map(|b| b.name()).join(",") ++ "\t" ++ if(empty, "empty", "changed") ++ "\t" ++ if(conflict, "conflict", "ok") ++ "\t" ++ description.first_line() ++ "\n""#;

/// Parse the two lines (`@`, then `@-`) printed with [`LOG_TEMPLATE`].
pub fn parse_status(out: &str) -> Option<JjStatus> {
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    let first = lines.next()?;
    let f: Vec<&str> = first.splitn(6, '\t').collect();
    if f.len() < 5 {
        return None;
    }
    let mut st = JjStatus {
        change_id: f[0].to_string(),
        commit_id: f[1].to_string(),
        bookmarks: f[2]
            .split(',')
            .filter(|b| !b.is_empty())
            .map(str::to_string)
            .collect(),
        empty: f[3] == "empty",
        conflict: f[4] == "conflict",
        description: f.get(5).unwrap_or(&"").to_string(),
        parent_change_id: None,
    };
    if let Some(parent) = lines.next() {
        let p: Vec<&str> = parent.splitn(6, '\t').collect();
        st.parent_change_id = p.first().map(|s| s.to_string());
        if let Some(bms) = p.get(2) {
            for b in bms.split(',').filter(|b| !b.is_empty()) {
                if !st.bookmarks.iter().any(|x| x == b) {
                    st.bookmarks.push(b.to_string());
                }
            }
        }
    }
    Some(st)
}

/// `jj workspace list` lines: `name: summary`.
pub fn parse_workspace_list(out: &str) -> Vec<JjWorkspace> {
    out.lines()
        .filter_map(|l| {
            let (n, rest) = l.split_once(':')?;
            Some(JjWorkspace {
                name: n.trim().to_string(),
                summary: rest.trim().to_string(),
            })
        })
        .collect()
}

impl Jj {
    fn run(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.bin)
            .arg("-R")
            .arg(dir)
            .args(["--no-pager", "--color=never"])
            .args(args)
            .env("JJ_EDITOR", "true")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    Error::Config(format!("jj not found ({})", self.bin.display()))
                }
                _ => Error::Io(e),
            })?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
        } else {
            Err(Error::Git {
                args: format!("jj {}", args.join(" ")),
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
    }

    /// `jj --version` works.
    pub fn available(&self) -> bool {
        Command::new(&self.bin)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Default base: `trunk()` when it is a real commit, else `@-`.
    pub fn default_base(&self, root: &Path) -> String {
        match self.run(
            root,
            &[
                "log",
                "--no-graph",
                "-r",
                "trunk() & ~root()",
                "-T",
                "commit_id",
                "--limit",
                "1",
            ],
        ) {
            Ok(s) if !s.trim().is_empty() => "trunk()".into(),
            _ => "@-".into(),
        }
    }

    /// Create a jj workspace for a task. `req.branch` (or the branch template) names the
    /// bookmark to create once there is a commit.
    pub fn create_workspace(&self, req: &CreateRequest, cfg: &WorktreeConfig) -> Result<Checkout> {
        let root = find_root(&req.repo).ok_or_else(|| Error::NotARepo(req.repo.clone()))?;
        let existing = self
            .run(&root, &["workspace", "list"])
            .map(|o| parse_workspace_list(&o))
            .unwrap_or_default();
        let slug_base = req
            .slug
            .clone()
            .unwrap_or_else(|| slugify(&req.title, cfg.slug_max_len));
        let mut chosen = None;
        for n in 1u32..10_000 {
            let slug = if n == 1 {
                slug_base.clone()
            } else {
                format!("{slug_base}-{n}")
            };
            let path = worktree_path(&cfg.root, &root, &slug);
            if path.exists() || existing.iter().any(|w| w.name == slug) {
                continue;
            }
            chosen = Some((slug, path));
            break;
        }
        let (slug, path) =
            chosen.ok_or_else(|| Error::Config("could not find a free slug".into()))?;
        let base = req.base.clone().unwrap_or_else(|| self.default_base(&root));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let path_s = path.to_string_lossy().into_owned();
        self.run(
            &root,
            &["workspace", "add", "--name", &slug, "-r", &base, &path_s],
        )?;
        let user = cfg.user.clone().unwrap_or_else(|| user_handle(&root));
        let branch = req
            .branch
            .clone()
            .unwrap_or_else(|| render_branch(&cfg.branch_template, &user, &slug));
        let mut warnings = Vec::new();
        if !is_colocated(&root) {
            warnings.push(
                "jj repo is not co-located with git: git tools won't see this workspace".into(),
            );
        }
        Ok(Checkout {
            path: path.canonicalize().unwrap_or(path),
            branch: Some(branch),
            base_ref: Some(base),
            slug,
            repo_root: root,
            created_branch: false,
            fetch: FetchOutcome::Skipped,
            warnings,
        })
    }

    pub fn status(&self, workspace_path: &Path) -> Result<JjStatus> {
        let out = self.run(
            workspace_path,
            &[
                "log",
                "--no-graph",
                "--ignore-working-copy",
                "-r",
                "@ | @-",
                "-T",
                LOG_TEMPLATE,
            ],
        )?;
        parse_status(&out).ok_or_else(|| Error::Config(format!("unexpected jj log output: {out}")))
    }

    pub fn list_workspaces(&self, root: &Path) -> Result<Vec<JjWorkspace>> {
        Ok(parse_workspace_list(
            &self.run(root, &["workspace", "list"])?,
        ))
    }

    /// Point bookmark `name` at `rev` (default `@-`, the last commit), creating it if needed.
    pub fn bookmark_set(&self, workspace_path: &Path, name: &str, rev: Option<&str>) -> Result<()> {
        self.run(
            workspace_path,
            &[
                "bookmark",
                "set",
                name,
                "-r",
                rev.unwrap_or("@-"),
                "--allow-backwards",
            ],
        )
        .map(|_| ())
    }

    pub fn forget_workspace(&self, root: &Path, name: &str) -> Result<()> {
        self.run(root, &["workspace", "forget", name]).map(|_| ())
    }

    /// Forget the workspace, then delete its directory (05 §9: rename aside first so the
    /// path is free at once).
    pub fn remove_workspace(&self, root: &Path, name: &str, path: &Path) -> Result<()> {
        self.forget_workspace(root, name)?;
        if path.exists() {
            let trash = path.with_file_name(format!(
                ".{}-trash-{}",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                ulid::Ulid::new()
            ));
            let victim = match std::fs::rename(path, &trash) {
                Ok(()) => trash,
                Err(_) => path.to_path_buf(),
            };
            std::fs::remove_dir_all(&victim)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn parsers() {
        let st = parse_status(
            "kxyz\tab12\t\tchanged\tok\twip: login\nqpar\tcd34\talice/login,main\tempty\tok\t\n",
        )
        .unwrap();
        assert_eq!(st.change_id, "kxyz");
        assert!(!st.empty && !st.conflict);
        assert_eq!(st.bookmarks, vec!["alice/login", "main"]);
        assert_eq!(st.parent_change_id.as_deref(), Some("qpar"));
        assert_eq!(st.description, "wip: login");
        assert!(parse_status("garbage").is_none());
        let ws = parse_workspace_list(
            "default: kxyz 1234 (empty) (no description set)\nfix-login: qq 55 wip\n",
        );
        assert_eq!(ws.len(), 2);
        assert_eq!(ws[1].name, "fix-login");
    }

    #[test]
    fn find_root_and_colocation() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".jj")).unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        let root = find_root(&d.path().join("a/b")).unwrap();
        assert_eq!(root, d.path().canonicalize().unwrap());
        assert!(!is_colocated(&root));
        std::fs::create_dir_all(d.path().join(".git")).unwrap();
        assert!(is_colocated(&root));
        let other = tempfile::tempdir().unwrap();
        assert!(find_root(other.path()).is_none());
    }

    /// A fake `jj` that logs its argv and emulates `workspace add` / `forget` / `list` / `log`.
    fn fake_jj(dir: &Path) -> (Jj, PathBuf) {
        let log = dir.join("jj.log");
        let bin = dir.join("jj");
        let script = format!(
            r#"#!/bin/sh
echo "$@" >> '{log}'
# drop -R <dir> --no-pager --color=never
shift 2; shift 2
case "$1 $2" in
  "workspace add") mkdir -p "$7"; echo "Created workspace in \"$7\"" ;;
  "workspace list") echo "default: aaa 111 (empty) (no description set)" ;;
  "workspace forget") ;;
  "bookmark set") ;;
  "log --no-graph")
     case "$*" in
       *trunk*) echo "deadbeef" ;;
       *) printf 'kxyz\tab12\t\tchanged\tok\twip\nqpar\tcd34\tme/x\tempty\tok\t\n' ;;
     esac ;;
  *) echo "unexpected: $*" >&2; exit 1 ;;
esac
"#,
            log = log.display()
        );
        std::fs::write(&bin, script).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        (Jj { bin }, log)
    }

    #[test]
    fn create_status_remove_with_fake_jj() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(repo.join(".jj")).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let (jj, log) = fake_jj(d.path());
        let cfg = WorktreeConfig {
            root: crate::WorktreeRoot::Dir(d.path().join("wt")),
            user: Some("me".into()),
            ..Default::default()
        };
        let co = jj
            .create_workspace(
                &CreateRequest {
                    repo: repo.clone(),
                    title: "Fix login".into(),
                    ..Default::default()
                },
                &cfg,
            )
            .unwrap();
        assert_eq!(co.slug, "fix-login");
        assert_eq!(co.branch.as_deref(), Some("me/fix-login"));
        assert_eq!(co.base_ref.as_deref(), Some("trunk()"));
        assert!(co.path.is_dir());
        assert!(co.warnings.is_empty());
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            calls.contains("workspace add --name fix-login -r trunk() "),
            "{calls}"
        );
        let st = jj.status(&co.path).unwrap();
        assert_eq!(st.bookmarks, vec!["me/x"]);
        jj.bookmark_set(&co.path, "me/fix-login", None).unwrap();
        jj.remove_workspace(&co.repo_root, &co.slug, &co.path)
            .unwrap();
        assert!(!co.path.exists());
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(calls.contains("workspace forget fix-login"), "{calls}");
        assert!(calls.contains("bookmark set me/fix-login -r @-"), "{calls}");
    }

    /// Real jj, only when installed (never installed by tests).
    #[test]
    fn real_jj_roundtrip() {
        let jj = Jj::default();
        if !jj.available() {
            eprintln!("jj not installed; skipping");
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let ok = Command::new(&jj.bin)
            .args(["git", "init", "--colocate"])
            .current_dir(&repo)
            .env("JJ_USER", "t")
            .env("JJ_EMAIL", "t@example.com")
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!("jj git init failed; skipping");
            return;
        }
        let cfg = WorktreeConfig {
            root: crate::WorktreeRoot::Dir(d.path().join("wt")),
            user: Some("t".into()),
            ..Default::default()
        };
        let co = jj
            .create_workspace(
                &CreateRequest {
                    repo: repo.clone(),
                    title: "Real jj".into(),
                    ..Default::default()
                },
                &cfg,
            )
            .unwrap();
        assert!(co.path.join(".jj").exists());
        assert!(
            jj.list_workspaces(&repo)
                .unwrap()
                .iter()
                .any(|w| w.name == co.slug)
        );
        let st = jj.status(&co.path).unwrap();
        assert!(!st.change_id.is_empty());
        jj.remove_workspace(&repo, &co.slug, &co.path).unwrap();
        assert!(
            !jj.list_workspaces(&repo)
                .unwrap()
                .iter()
                .any(|w| w.name == co.slug)
        );
    }
}
