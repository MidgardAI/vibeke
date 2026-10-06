//! Repo-local `.vibeke/config.toml` on the server side (08 §11.1, 09 §4), the smallest piece
//! the TUI and `vibeke trust` need:
//!
//! - `policy.trust {path, check: true}` reports the repo file and whether the repo's current
//!   `.vibeke/` digest is trusted, without recording anything (hooked in `run.rs`).
//! - [`tasks_cfg`]: `[tasks]` for `task.create`, with a **trusted** repo file's `[tasks]` keys
//!   layered over the user's config. An untrusted or changed file is ignored.
//!
//! Trust itself stays `policy.trust` (digest of the whole `.vibeke/` tree, so any edit to the
//! file needs a new review).

use crate::Server;
use crate::api::{R, s};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The repository root for `path` (git root, else the directory holding `.vibeke/config.toml`).
fn root_of(path: &Path) -> PathBuf {
    let base = vk_tasks::repo_root(path)
        .map(|i| i.root)
        .or_else(|| vk_config::repo::find(path))
        .unwrap_or_else(|| path.to_path_buf());
    base.canonicalize().unwrap_or(base)
}

/// Whether `root`'s `.vibeke/` tree is trusted as it is now.
pub fn trusted(server: &Server, root: &Path) -> (Option<String>, bool) {
    let digest = crate::run::vibeke_dir_digest(root);
    let ok = digest
        .as_deref()
        .is_some_and(|d| crate::run::repo_trusted(server, root, d));
    (digest, ok)
}

/// `policy.trust {path, check: true}`: the repo file, its digest and trust state.
pub fn check(server: &Server, p: &Value) -> R {
    let path = PathBuf::from(s(p, "path").unwrap_or("."));
    let root = root_of(&path);
    let (digest, ok) = trusted(server, &root);
    let file = root.join(vk_config::REPO_CONFIG);
    let parsed = vk_config::repo::load(&root);
    let (text, warnings, commands, error) = match &parsed {
        None => (None, vec![], vec![], None),
        Some(Ok(rc)) => (
            Some(rc.text.clone()),
            rc.warnings.clone(),
            rc.commands
                .iter()
                .map(|c| json!({"key": c.key, "type": c.kind.as_str(), "command": c.command, "title": c.title, "width": c.width, "height": c.height}))
                .collect(),
            None,
        ),
        Some(Err(e)) => (None, vec![], vec![], Some(e.clone())),
    };
    Ok(json!({
        "repo": root,
        "file": parsed.is_some().then_some(file),
        "digest": digest,
        "trusted": ok,
        "text": text,
        "warnings": warnings,
        "commands": commands,
        "error": error,
    }))
}

/// `[tasks]` for a task in `repo`: the user's config with a trusted repo file layered on top.
pub fn tasks_cfg(server: &Server, repo: &Path) -> vk_config::Tasks {
    let user = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    let root = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    if let Some(Ok(rc)) = vk_config::repo::load(&root)
        && trusted(server, &root).1
        && let Ok((c, _)) = vk_config::Config::load_layered(&vk_config::config_path(), Some(&rc))
    {
        return c.tasks;
    }
    user.tasks
}
