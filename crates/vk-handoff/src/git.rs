//! git on this host. Same hardening as the server's git methods: repo-configured programs never
//! run (spec 16 §7.7).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use crate::{Error, Result};

const TIMEOUT: Duration = Duration::from_secs(120);
/// `fetch` and `clone` move whole histories.
const LONG_TIMEOUT: Duration = Duration::from_secs(600);

/// The user's umask for git children and the files an import writes (09 §3.1): a host that runs
/// with `umask 077` still creates checkouts with the user's own mode bits. `u32::MAX` = unset
/// (inherit).
static CHILD_UMASK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);

/// Set the umask git children run with and imported files are created with.
pub fn set_child_umask(mask: u32) {
    CHILD_UMASK.store(mask & 0o777, std::sync::atomic::Ordering::Relaxed);
}

/// The umask set by [`set_child_umask`], if any.
pub(crate) fn child_umask() -> Option<u32> {
    let m = CHILD_UMASK.load(std::sync::atomic::Ordering::Relaxed);
    (m != u32::MAX).then_some(m)
}

/// `-c filter.<name>.{clean,smudge,process}=` for every configured filter driver (reading config
/// runs nothing; an empty command disables the filter).
async fn filter_overrides(dir: &Path) -> Vec<String> {
    let out = tokio::process::Command::new("git")
        .args([
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            r"^filter\.",
        ])
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await;
    let Ok(out) = out else { return Vec::new() };
    let mut names: Vec<String> = out
        .stdout
        .split(|&b| b == 0)
        .filter_map(|k| {
            String::from_utf8_lossy(k)
                .strip_prefix("filter.")
                .and_then(|r| r.rsplit_once('.'))
                .map(|(n, _)| n.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
        .iter()
        .flat_map(|n| {
            ["clean", "smudge", "process"]
                .iter()
                .flat_map(move |k| ["-c".to_string(), format!("filter.{n}.{k}=")])
                .chain(["-c".to_string(), format!("filter.{n}.required=false")])
        })
        .collect()
}

/// Run git in `dir`; stdout on success, `conflict` with stderr otherwise.
pub async fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let filters = filter_overrides(dir).await;
    let timeout = match args.first() {
        Some(&"fetch" | &"clone") => LONG_TIMEOUT,
        _ => TIMEOUT,
    };
    let mut cmd = tokio::process::Command::new("git");
    if let Some(m) = child_umask() {
        // SAFETY: umask is async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
                libc::umask(m as libc::mode_t);
                Ok(())
            });
        }
    }
    let out = tokio::time::timeout(
        timeout,
        cmd.arg("--literal-pathspecs")
            .args(&filters)
            .args([
                "-c",
                "submodule.recurse=false",
                "-c",
                "diff.ignoreSubmodules=all",
                "-c",
                "status.submoduleSummary=false",
            ])
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "diff.external=",
                "-c",
                "core.pager=cat",
                "-c",
                "color.ui=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_EXTERNAL_DIFF")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        Error::new(
            "timeout",
            format!("git {} timed out", args.first().unwrap_or(&"")),
        )
    })?
    .map_err(|e| Error::new("unsupported", format!("git: {e}")))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(Error::new(
            "conflict",
            format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ))
    }
}

/// Trimmed stdout, `None` on failure or empty output.
pub async fn git_line(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args)
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .filter(|s| !s.is_empty())
}
