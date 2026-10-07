//! Host directory browsing for path pickers: `fs.browse` and `repo.candidates`.
//!
//! Unlike `fs.list` (repository-relative, limited to a pane's working tree), `fs.browse` walks
//! the host's directories so a client can choose where a repository or worktree goes. It is
//! confined to the allowed roots: `$HOME` plus the `[handoff] roots` from config.toml. Paths
//! are canonicalized before the check, so `..` and symlinks can't leave the roots; a symlinked
//! child is listed only when its target is a directory inside them. Only directories are
//! listed, at most [`MAX_ENTRIES`].
//!
//! `repo.candidates` finds clones of a remote: the open workspaces' repositories and a shallow
//! scan of the usual source folders, bounded in time and directories visited.
//!
//! Both are full scope only (they see the whole home directory).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vk_proto::rpc::{ErrorKind, RpcError};

use crate::Server;
use crate::api::{R, err, internal, invalid, req, s};

pub const METHODS: &[(&str, bool)] = &[("fs.browse", false), ("repo.candidates", false)];

/// Host-wide views: never for pane tokens.
pub const PANE_FORBIDDEN: &[&str] = &["fs.browse", "repo.candidates"];

/// Most directories one `fs.browse` returns.
pub const MAX_ENTRIES: usize = 500;
/// Folders under `$HOME` that `repo.candidates` scans (two levels deep) when they exist.
pub const SCAN_DIRS: &[&str] = &["code", "src", "dev", "projects", "kode", "work", "git"];
const SCAN_DEPTH: usize = 2;
const SCAN_MAX_DIRS: usize = 2000;
const SCAN_TIME: Duration = Duration::from_secs(2);

pub async fn api(server: &Arc<Server>, method: &str, p: &Value) -> Option<R> {
    if !matches!(method, "fs.browse" | "repo.candidates") {
        return None;
    }
    let p = p.clone();
    Some(match method {
        "fs.browse" => {
            let path = s(&p, "path").unwrap_or("~").to_string();
            let prefix = s(&p, "prefix").unwrap_or("").to_string();
            tokio::task::spawn_blocking(move || {
                let home = crate::paths::home();
                let roots = allowed_roots(&home, &config_roots());
                browse(&home, &roots, &path, &prefix)
            })
            .await
            .map_err(internal)
            .and_then(|r| r)
        }
        _ => {
            let origin = match req(&p, "origin") {
                Ok(o) => o.to_string(),
                Err(e) => return Some(Err(e)),
            };
            let workspaces: Vec<PathBuf> = server.with_core(|c| {
                c.model
                    .workspaces
                    .iter()
                    .map(|w| PathBuf::from(&w.root_path))
                    .collect()
            });
            tokio::task::spawn_blocking(move || {
                let home = crate::paths::home();
                let extra = config_roots();
                candidates(&home, &extra, &workspaces, &origin)
            })
            .await
            .map_err(internal)
            .and_then(|r| r)
        }
    })
}

/// `[handoff] roots = ["/Volumes/work", "~/elsewhere"]` from config.toml (absolute or `~`).
fn config_roots() -> Vec<PathBuf> {
    let Ok((cfg, _)) = vk_config::Config::load(vk_config::config_path()) else {
        return vec![];
    };
    let home = crate::paths::home();
    cfg.extra
        .get("handoff")
        .and_then(|h| h.get("roots"))
        .and_then(|r| r.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .map(|r| expand_home(&home, r))
        .filter(|p| p.is_absolute())
        .collect()
}

/// `$HOME` plus `extra`, canonicalized; roots that don't exist are dropped.
pub fn allowed_roots(home: &Path, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![];
    for r in std::iter::once(home).chain(extra.iter().map(PathBuf::as_path)) {
        if let Ok(c) = std::fs::canonicalize(r)
            && c.is_dir()
            && !out.contains(&c)
        {
            out.push(c);
        }
    }
    out
}

pub fn expand_home(home: &Path, p: &str) -> PathBuf {
    if p == "~" {
        home.to_path_buf()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(p)
    }
}

fn inside(roots: &[PathBuf], p: &Path) -> bool {
    roots.iter().any(|r| p.starts_with(r))
}

fn denied(path: &str) -> RpcError {
    err(
        ErrorKind::PermissionDenied,
        format!("{path} is outside the folders you can browse"),
    )
    .details(json!({"path": path}))
}

/// Does `dir` hold a `.git` (directory, or the file of a linked worktree / submodule)?
pub fn is_git_repo(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join(".git")).is_ok()
}

/// List the directories in `path` (see the module docs). `prefix` filters names
/// (case-insensitive); dot-directories are hidden unless it starts with `.`.
pub fn browse(home: &Path, roots: &[PathBuf], path: &str, prefix: &str) -> R {
    if path.len() > 4096 || path.contains('\0') {
        return Err(invalid("path is too long or malformed"));
    }
    let path = if path.is_empty() { "~" } else { path };
    let expanded = expand_home(home, path);
    if !expanded.is_absolute() {
        return Err(invalid("path must be absolute or start with ~"));
    }
    let dir = match std::fs::canonicalize(&expanded) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(err(ErrorKind::NotFound, format!("no such folder: {path}"))
                .details(json!({"object": "path", "target": path})));
        }
        Err(e) => return Err(internal(e)),
    };
    if !inside(roots, &dir) {
        return Err(denied(path));
    }
    if !dir.is_dir() {
        return Err(invalid(format!("not a folder: {path}")));
    }
    let show_dot = prefix.starts_with('.');
    let want = prefix.to_lowercase();
    let mut entries: Vec<(String, bool)> = vec![];
    for e in std::fs::read_dir(&dir).map_err(internal)?.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with('.') && !show_dot {
            continue;
        }
        if !want.is_empty() && !name.to_lowercase().starts_with(&want) {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        let target = if ft.is_dir() {
            e.path()
        } else if ft.is_symlink() {
            // Followed only when it lands on a directory inside the roots.
            match std::fs::canonicalize(e.path()) {
                Ok(t) if t.is_dir() && inside(roots, &t) => t,
                _ => continue,
            }
        } else {
            continue;
        };
        entries.push((name, is_git_repo(&target)));
    }
    entries.sort_by(|a, b| {
        a.0.to_lowercase()
            .cmp(&b.0.to_lowercase())
            .then_with(|| a.0.cmp(&b.0))
    });
    let truncated = entries.len() > MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    let parent = dir
        .parent()
        .filter(|p| inside(roots, p))
        .map(|p| p.to_string_lossy().into_owned());
    Ok(json!({
        "path": dir.to_string_lossy(),
        "parent": parent,
        "git_repo": is_git_repo(&dir),
        "entries": entries
            .into_iter()
            .map(|(name, git)| json!({"name": name, "git_repo": git}))
            .collect::<Vec<_>>(),
        "truncated": truncated,
    }))
}

// ---- repo.candidates -----------------------------------------------------------------------

/// `(host, path)` of a git remote: URL form `scheme://[user@]host[:port]/path` or scp form
/// `[user@]host:path`. Host is case-insensitive; the path is not.
///
/// Mirrors `remote_parts` in `crates/vk-gateway/src/handoff.rs` (moving to the shared
/// `vk-handoff` crate); keep the two identical until they are deduplicated.
pub fn remote_parts(u: &str) -> Option<(String, String)> {
    let u = u.trim();
    // Local repositories: an absolute path or file:// URL, compared exactly.
    if let Some(path) = u
        .strip_prefix("file://")
        .or(u.starts_with('/').then_some(u))
    {
        let path = path.trim_end_matches('/').trim_end_matches(".git");
        return (!path.is_empty()).then(|| (String::new(), path.to_string()));
    }
    let (host, path) = if let Some((_, rest)) = u.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let host = host.split(':').next()?;
        (host, path)
    } else {
        let (left, path) = u.split_once(':')?;
        if left.contains('/') {
            return None; // a local path, not scp syntax
        }
        (left.rsplit_once('@').map_or(left, |(_, h)| h), path)
    };
    if host.is_empty() || host.contains('@') {
        return None;
    }
    let path = path.trim_matches('/').trim_end_matches(".git").to_string();
    (!path.is_empty() && !path.contains('@')).then(|| (host.to_ascii_lowercase(), path))
}

/// Mirrors `same_remote` in `crates/vk-gateway/src/handoff.rs`.
pub fn same_remote(a: &str, b: &str) -> bool {
    matches!((remote_parts(a), remote_parts(b)), (Some(x), Some(y)) if x == y)
}

/// The repository's git directory holding `config`: `.git`, or for a linked worktree the
/// `gitdir:` it points at, then its `commondir`.
fn git_common_dir(repo: &Path) -> Option<PathBuf> {
    let dot = repo.join(".git");
    let meta = std::fs::symlink_metadata(&dot).ok()?;
    let gitdir = if meta.is_dir() {
        dot
    } else {
        let text = std::fs::read_to_string(&dot).ok()?;
        let rel = text.lines().next()?.strip_prefix("gitdir:")?.trim();
        repo.join(rel)
    };
    match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(c) => Some(gitdir.join(c.trim())),
        Err(_) => Some(gitdir),
    }
}

/// `url`s of every `[remote "…"]` section in the repository's config.
pub fn remote_urls(repo: &Path) -> Vec<String> {
    let Some(dir) = git_common_dir(repo) else {
        return vec![];
    };
    let Ok(text) = std::fs::read_to_string(dir.join("config")) else {
        return vec![];
    };
    let mut in_remote = false;
    let mut out = vec![];
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_remote = l.starts_with("[remote ");
            continue;
        }
        if !in_remote {
            continue;
        }
        if let Some((k, v)) = l.split_once('=')
            && k.trim().eq_ignore_ascii_case("url")
        {
            out.push(v.trim().trim_matches('"').to_string());
        }
    }
    out
}

/// The nearest enclosing directory (or `p` itself) holding `.git`.
fn repo_top(p: &Path) -> Option<PathBuf> {
    p.ancestors()
        .find(|a| is_git_repo(a))
        .map(Path::to_path_buf)
}

/// Repositories under `root`, at most [`SCAN_DEPTH`] levels down; symlinks and dot-directories
/// are skipped, and a repository's own subdirectories are not entered.
fn scan(root: &Path, budget: &mut (usize, Instant), out: &mut Vec<PathBuf>) {
    let mut level = vec![root.to_path_buf()];
    for _ in 0..SCAN_DEPTH {
        let mut next = vec![];
        for dir in level {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                if budget.0 >= SCAN_MAX_DIRS || budget.1.elapsed() >= SCAN_TIME {
                    return;
                }
                if e.file_name().to_string_lossy().starts_with('.')
                    || !e.file_type().is_ok_and(|t| t.is_dir())
                {
                    continue;
                }
                budget.0 += 1;
                let p = e.path();
                if is_git_repo(&p) {
                    out.push(p);
                } else {
                    next.push(p);
                }
            }
        }
        level = next;
    }
}

/// Clones whose any remote is `origin` (see the module docs).
pub fn candidates(home: &Path, extra_roots: &[PathBuf], workspaces: &[PathBuf], origin: &str) -> R {
    if remote_parts(origin).is_none() {
        return Err(invalid(format!("not a git remote: {origin}")));
    }
    let mut repos: Vec<PathBuf> = workspaces.iter().filter_map(|w| repo_top(w)).collect();
    let mut budget = (0usize, Instant::now());
    let scan_roots = SCAN_DIRS
        .iter()
        .map(|d| home.join(d))
        .chain(extra_roots.iter().cloned());
    for root in scan_roots {
        if root.is_dir() {
            scan(&root, &mut budget, &mut repos);
        }
    }
    let mut seen = HashSet::new();
    let mut out = vec![];
    for repo in repos {
        let key = std::fs::canonicalize(&repo).unwrap_or_else(|_| repo.clone());
        if !seen.insert(key.clone()) {
            continue;
        }
        if let Some(url) = remote_urls(&key)
            .into_iter()
            .find(|u| same_remote(u, origin))
        {
            out.push(json!({"path": key.to_string_lossy(), "remote": url}));
        }
    }
    Ok(json!({"repos": out}))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fx {
        _t: tempfile::TempDir,
        home: PathBuf,
        outside: PathBuf,
    }

    fn fx() -> Fx {
        let t = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(t.path()).unwrap();
        let home = base.join("home");
        let outside = base.join("outside");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(outside.join("secret")).unwrap();
        Fx {
            _t: t,
            home,
            outside,
        }
    }

    fn names(v: &Value) -> Vec<String> {
        v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect()
    }

    fn kind(r: R) -> String {
        r.unwrap_err().data.kind
    }

    #[test]
    fn lists_directories_sorted_without_files_or_dotdirs() {
        let f = fx();
        for d in ["beta", "Alpha", "gamma", ".hidden"] {
            std::fs::create_dir(f.home.join(d)).unwrap();
        }
        std::fs::write(f.home.join("file.txt"), "x").unwrap();
        let roots = allowed_roots(&f.home, &[]);
        let r = browse(&f.home, &roots, "~", "").unwrap();
        assert_eq!(names(&r), ["Alpha", "beta", "gamma"]);
        assert_eq!(r["path"], &*f.home.to_string_lossy());
        // $HOME's parent is outside the roots.
        assert!(r["parent"].is_null());
        assert_eq!(r["truncated"], false);

        let r = browse(&f.home, &roots, "~/", ".").unwrap();
        assert_eq!(names(&r), [".hidden"]);
        let r = browse(&f.home, &roots, &f.home.to_string_lossy(), "AL").unwrap();
        assert_eq!(names(&r), ["Alpha"]);
        let r = browse(&f.home, &roots, "~/beta", "").unwrap();
        assert_eq!(r["parent"], &*f.home.to_string_lossy());
    }

    #[test]
    fn marks_git_repositories() {
        let f = fx();
        std::fs::create_dir_all(f.home.join("repo/.git")).unwrap();
        std::fs::create_dir_all(f.home.join("worktree")).unwrap();
        std::fs::write(
            f.home.join("worktree/.git"),
            "gitdir: ../repo/.git/worktrees/w\n",
        )
        .unwrap();
        std::fs::create_dir(f.home.join("plain")).unwrap();
        let roots = allowed_roots(&f.home, &[]);
        let r = browse(&f.home, &roots, "~", "").unwrap();
        let git: Vec<(String, bool)> = r["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["name"].as_str().unwrap().into(),
                    e["git_repo"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            git,
            [
                ("plain".into(), false),
                ("repo".into(), true),
                ("worktree".into(), true)
            ]
        );
        let r = browse(&f.home, &roots, "~/repo", "").unwrap();
        assert_eq!(r["git_repo"], true);
    }

    #[test]
    fn refuses_paths_outside_the_roots() {
        let f = fx();
        std::fs::create_dir(f.home.join("a")).unwrap();
        let roots = allowed_roots(&f.home, &[]);
        let up = format!("{}/a/../..", f.home.display());
        assert_eq!(kind(browse(&f.home, &roots, &up, "")), "permission_denied");
        assert_eq!(
            kind(browse(&f.home, &roots, "~/..", "")),
            "permission_denied"
        );
        assert_eq!(kind(browse(&f.home, &roots, "/", "")), "permission_denied");
        assert_eq!(
            kind(browse(&f.home, &roots, &f.outside.to_string_lossy(), "")),
            "permission_denied"
        );
        assert_eq!(
            kind(browse(&f.home, &roots, "relative", "")),
            "invalid_params"
        );
        assert_eq!(kind(browse(&f.home, &roots, "~/missing", "")), "not_found");
        // An extra root is browsable.
        let roots = allowed_roots(&f.home, std::slice::from_ref(&f.outside));
        let r = browse(&f.home, &roots, &f.outside.to_string_lossy(), "").unwrap();
        assert_eq!(names(&r), ["secret"]);
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_a_symlink_out_of_the_roots() {
        let f = fx();
        std::fs::create_dir(f.home.join("inside")).unwrap();
        std::os::unix::fs::symlink(&f.outside, f.home.join("escape")).unwrap();
        std::os::unix::fs::symlink(f.home.join("inside"), f.home.join("alias")).unwrap();
        std::os::unix::fs::symlink(f.home.join("missing"), f.home.join("dangling")).unwrap();
        let roots = allowed_roots(&f.home, &[]);
        let r = browse(&f.home, &roots, "~", "").unwrap();
        // The escaping and dangling links are not listed; one inside is.
        assert_eq!(names(&r), ["alias", "inside"]);
        assert_eq!(
            kind(browse(&f.home, &roots, "~/escape", "")),
            "permission_denied"
        );
        assert_eq!(
            kind(browse(&f.home, &roots, "~/escape/secret", "")),
            "permission_denied"
        );
    }

    #[test]
    fn caps_the_listing() {
        let f = fx();
        for i in 0..MAX_ENTRIES + 3 {
            std::fs::create_dir(f.home.join(format!("d{i:04}"))).unwrap();
        }
        let roots = allowed_roots(&f.home, &[]);
        let r = browse(&f.home, &roots, "~", "").unwrap();
        assert_eq!(r["entries"].as_array().unwrap().len(), MAX_ENTRIES);
        assert_eq!(r["truncated"], true);
    }

    fn clone_at(dir: &Path, url: &str) {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(
            dir.join(".git/config"),
            format!(
                "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = https://example.com/other/x\n[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn remotes_compare_by_host_and_path() {
        assert!(same_remote(
            "git@GitHub.com:a/b.git",
            "https://github.com/a/b"
        ));
        assert!(same_remote(
            "ssh://git@github.com:22/a/b/",
            "github.com:a/b"
        ));
        assert!(!same_remote(
            "https://github.com/a/b",
            "https://github.com/a/c"
        ));
        assert!(remote_parts("not a remote").is_none());
    }

    #[test]
    fn finds_matching_clones_in_scan_dirs_and_workspaces() {
        let f = fx();
        clone_at(
            &f.home.join("code/vibeke"),
            "git@github.com:acme/vibeke.git",
        );
        clone_at(
            &f.home.join("src/acme/vibeke-2"),
            "https://github.com/acme/vibeke",
        );
        clone_at(&f.home.join("code/other"), "https://github.com/acme/other");
        // Three levels down: beyond the scan.
        clone_at(
            &f.home.join("dev/a/b/deep"),
            "https://github.com/acme/vibeke",
        );
        // A workspace outside the scan dirs, opened in a subdirectory of the clone.
        let ws = f.outside.join("ws");
        clone_at(&ws, "https://github.com/acme/vibeke.git");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        let r = candidates(
            &f.home,
            &[],
            &[ws.join("sub"), f.home.join("code/vibeke")],
            "https://github.com/acme/vibeke",
        )
        .unwrap();
        let mut got: Vec<String> = r["repos"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["path"].as_str().unwrap().to_string())
            .collect();
        got.sort();
        let mut want = vec![
            ws.to_string_lossy().into_owned(),
            f.home.join("code/vibeke").to_string_lossy().into_owned(),
            f.home
                .join("src/acme/vibeke-2")
                .to_string_lossy()
                .into_owned(),
        ];
        want.sort();
        assert_eq!(got, want);
        assert_eq!(
            kind(candidates(&f.home, &[], &[], "nonsense")),
            "invalid_params"
        );
    }

    #[test]
    fn reads_a_linked_worktree_through_its_common_dir() {
        let f = fx();
        let main = f.home.join("code/main");
        clone_at(&main, "https://github.com/acme/vibeke");
        let wt_git = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let wt = f.home.join("code/wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        assert_eq!(
            remote_urls(&wt),
            [
                "https://example.com/other/x",
                "https://github.com/acme/vibeke"
            ]
        );
    }
}
