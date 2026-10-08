//! Automatic workspace names. A workspace without an explicit `name` is shown under an
//! `auto_name` that follows its focused pane's current directory: the basename of the
//! enclosing git repository (or worktree) top level, else the path with `$HOME` as `~`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(10);
const MAX_ENTRIES: usize = 512;

type GitCache = HashMap<String, (Instant, Option<PathBuf>)>;

fn cache() -> &'static Mutex<GitCache> {
    static C: OnceLock<Mutex<GitCache>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// The nearest ancestor of `dir` (itself included) that holds a `.git` file (linked
/// worktree) or directory. No subprocess; results are cached per directory for a few seconds.
pub fn git_root(dir: &str) -> Option<PathBuf> {
    let now = Instant::now();
    if let Some((at, r)) = cache().lock().unwrap().get(dir)
        && now.duration_since(*at) < TTL
    {
        return r.clone();
    }
    let found = Path::new(dir)
        .ancestors()
        .find(|a| a.join(".git").exists())
        .map(Path::to_path_buf);
    let mut c = cache().lock().unwrap();
    if c.len() >= MAX_ENTRIES {
        c.clear();
    }
    c.insert(dir.to_string(), (now, found.clone()));
    found
}

/// The name for `cwd` given the server machine's `home` and the git top level of `cwd`.
pub fn name_for(cwd: &str, home: &Path, git_top: Option<&Path>) -> String {
    if let Some(top) = git_top
        && let Some(n) = top.file_name()
    {
        return n.to_string_lossy().into_owned();
    }
    let p = Path::new(cwd);
    if p == home && home != Path::new("/") {
        return "~".into();
    }
    if home != Path::new("/")
        && let Ok(rest) = p.strip_prefix(home)
    {
        return format!("~/{}", rest.display());
    }
    cwd.to_string()
}

/// [`name_for`] with the real home and a (cached) git lookup.
pub fn auto_name(cwd: &str) -> String {
    let top = git_root(cwd);
    name_for(cwd, &crate::paths::home(), top.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/espen")
    }

    #[test]
    fn home_is_tilde_and_subdirs_are_abbreviated() {
        assert_eq!(name_for("/home/espen", &home(), None), "~");
        assert_eq!(name_for("/home/espen/notes/a", &home(), None), "~/notes/a");
        assert_eq!(name_for("/srv/data", &home(), None), "/srv/data");
        assert_eq!(name_for("/home/espenx", &home(), None), "/home/espenx");
    }

    #[test]
    fn root_stays_root() {
        assert_eq!(name_for("/", &home(), None), "/");
        assert_eq!(name_for("/", Path::new("/"), None), "/");
    }

    #[test]
    fn git_top_level_basename_wins() {
        let top = PathBuf::from("/home/espen/code/vibeke");
        assert_eq!(
            name_for("/home/espen/code/vibeke", &home(), Some(&top)),
            "vibeke"
        );
        assert_eq!(
            name_for("/home/espen/code/vibeke/crates/x", &home(), Some(&top)),
            "vibeke"
        );
    }

    #[test]
    fn finds_repo_dir_nested_dirs_and_worktree_file() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("a/b")).unwrap();
        assert_eq!(
            git_root(repo.to_str().unwrap()).as_deref(),
            Some(repo.as_path())
        );
        assert_eq!(
            git_root(repo.join("a/b").to_str().unwrap()).as_deref(),
            Some(repo.as_path())
        );
        let wt = t.path().join("wt");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(
            git_root(wt.join("src").to_str().unwrap()).as_deref(),
            Some(wt.as_path())
        );
        let plain = t.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        // The tempdir itself is not in a repo (unless /tmp is; tolerate that).
        if git_root(t.path().to_str().unwrap()).is_none() {
            assert_eq!(git_root(plain.to_str().unwrap()), None);
        }
    }
}
