//! Git configuration that makes the host execute files (13 §6): `core.hooksPath`,
//! `core.fsmonitor`, filter/diff/merge drivers, editors, credential helpers, `!` aliases and
//! the files pulled in by `include.path`/`includeIf.*.path`. When any of those resolve **inside
//! the task checkout**, the contained process could rewrite them without touching `.git/config`
//! and the next host git command (the user's own, or Vibeke's) would run the new code. The
//! sandbox therefore treats every such target as protected: never writable (Seatbelt deny,
//! read-only bind on Linux and in containers).
//!
//! Reading configuration with `git config --list` executes nothing.

use std::path::{Component, Path, PathBuf};

/// Lexically normalize `p` (resolve `.`/`..` without touching the filesystem).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

fn expand(raw: &str, base: &Path, home: Option<&Path>) -> PathBuf {
    let raw = raw.trim_matches(|c| c == '"' || c == '\'');
    let p = match (raw.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(raw),
    };
    if p.is_absolute() {
        normalize(&p)
    } else {
        normalize(&base.join(p))
    }
}

fn is_boolish(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "" | "true" | "false" | "yes" | "no" | "on" | "off" | "1" | "0"
    )
}

/// Path-like words of a command value (`sh ./x.sh --flag` → `./x.sh`); `!` alias prefixes and
/// quotes are stripped. Bare names are looked up on `PATH`, not in the checkout, and skipped.
fn command_paths(v: &str) -> Vec<String> {
    v.trim_start_matches('!')
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c| c == '"' || c == '\'' || c == ';')
                .to_string()
        })
        .filter(|w| w.contains('/') && !w.starts_with('-'))
        .collect()
}

/// Does `key` (lowercased by git) name a value git executes?
fn is_command_key(key: &str) -> bool {
    let parts: Vec<&str> = key.split('.').collect();
    let (section, name) = (parts[0], parts[parts.len() - 1]);
    matches!(
        (section, name, parts.len()),
        (
            "core",
            "fsmonitor" | "sshcommand" | "editor" | "pager" | "askpass" | "alternaterefscommand",
            2
        ) | ("sequence", "editor", 2)
            | ("diff", "external", 2)
            | ("gpg", "program", _)
            | ("credential", "helper", _)
            | ("filter", "clean" | "smudge" | "process", 3..)
            | ("diff", "textconv" | "command", 3..)
            | ("merge", "driver", 3..)
            | ("uploadpack", "packobjectshook", 2)
            | ("alias", _, _)
    )
}

/// Protected targets from `git config --list --show-origin --includes -z` output. `checkout` is
/// the worktree root (relative hook paths and commands resolve against it); only results inside
/// it are returned.
pub fn targets_from_config(z: &[u8], checkout: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let checkout = normalize(checkout);
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if p.starts_with(&checkout) && p != checkout && !out.contains(&p) {
            out.push(p);
        }
    };
    let fields: Vec<String> = z
        .split(|&b| b == 0)
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect();
    let mut i = 0;
    while i + 1 < fields.len() {
        let origin = &fields[i];
        let entry = &fields[i + 1];
        i += 2;
        // `git -C <checkout>` prints repo-local origins relative to the checkout
        // (`file:.git/config`, `file:.git/../inc.cfg`).
        let origin_file = origin
            .strip_prefix("file:")
            .map(|f| normalize(&checkout.join(f)));
        if let Some(f) = &origin_file {
            // A config file that lives in the checkout (an include target) is itself protected.
            push(normalize(f));
        }
        let (key, value) = entry.split_once('\n').unwrap_or((entry.as_str(), ""));
        let key = key.to_ascii_lowercase();
        let config_dir = origin_file
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| checkout.clone());
        if key == "core.hookspath" {
            push(expand(value, &checkout, home));
        } else if key == "include.path" || (key.starts_with("includeif.") && key.ends_with(".path"))
        {
            push(expand(value, &config_dir, home));
        } else if key == "core.fsmonitor" && is_boolish(value) {
            // The builtin daemon or off.
        } else if is_command_key(&key) {
            if key.starts_with("alias.") && !value.trim_start().starts_with('!') {
                continue;
            }
            for w in command_paths(value) {
                push(expand(&w, &checkout, home));
            }
        }
    }
    out
}

/// Ask git for the configuration of `checkout` and return the protected targets inside it.
pub fn exec_targets(checkout: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["config", "--list", "--show-origin", "--includes", "-z"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(out) = out else { return vec![] };
    let co = checkout
        .canonicalize()
        .unwrap_or_else(|_| checkout.to_path_buf());
    let mut v = targets_from_config(&out.stdout, &co, home);
    // Paths git printed may be spelled through a symlinked ancestor of the checkout.
    if co != checkout {
        for p in targets_from_config(&out.stdout, checkout, home) {
            if let Ok(rel) = p.strip_prefix(checkout) {
                let q = co.join(rel);
                if !v.contains(&q) {
                    v.push(q);
                }
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(entries: &[(&str, &str, &str)]) -> Vec<u8> {
        let mut v = Vec::new();
        for (o, k, val) in entries {
            v.extend_from_slice(o.as_bytes());
            v.push(0);
            v.extend_from_slice(format!("{k}\n{val}").as_bytes());
            v.push(0);
        }
        v
    }

    #[test]
    fn parses_hook_fsmonitor_drivers_aliases_and_includes() {
        let co = Path::new("/w/repo-task");
        let cfg = z(&[
            ("file:/w/repo/.git/config", "core.hookspath", ".githooks"),
            (
                "file:/w/repo/.git/config",
                "core.fsmonitor",
                "./tools/fsmon.sh",
            ),
            ("file:/w/repo/.git/config", "core.fsmonitor", "true"),
            (
                "file:/w/repo/.git/config",
                "filter.lfs.process",
                "git-lfs filter-process",
            ),
            (
                "file:/w/repo/.git/config",
                "filter.x.clean",
                "sh scripts/clean.sh --fast",
            ),
            ("file:/w/repo/.git/config", "alias.up", "!./bin/up arg"),
            (
                "file:/w/repo/.git/config",
                "alias.st",
                "status ./not-a-command",
            ),
            (
                "file:/w/repo/.git/config",
                "include.path",
                "../../repo-task/.gitconfig.local",
            ),
            ("file:/w/repo-task/.gitconfig.local", "user.name", "x"),
            (
                "file:/w/repo/.git/config",
                "diff.pdf.textconv",
                "/usr/bin/pdftotext",
            ),
            ("command line:", "core.editor", "/w/repo-task/ed"),
        ]);
        let t = targets_from_config(&cfg, co, None);
        for want in [
            "/w/repo-task/.githooks",
            "/w/repo-task/tools/fsmon.sh",
            "/w/repo-task/scripts/clean.sh",
            "/w/repo-task/bin/up",
            "/w/repo-task/.gitconfig.local",
            "/w/repo-task/ed",
        ] {
            assert!(
                t.contains(&PathBuf::from(want)),
                "{want} missing from {t:?}"
            );
        }
        assert!(!t.iter().any(|p| p.ends_with("not-a-command")));
        assert!(!t.iter().any(|p| p.starts_with("/usr")));
        assert!(!t.contains(&co.to_path_buf()));
    }

    #[test]
    fn real_git_config_targets() {
        let t = tempfile::tempdir().unwrap();
        let co = t.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(&co).unwrap();
        let g = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&co)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap()
                .success();
            assert!(ok, "{args:?}");
        };
        g(&["init", "-q"]);
        g(&["config", "core.hooksPath", ".githooks"]);
        g(&["config", "core.fsmonitor", "./fsmon"]);
        g(&["config", "include.path", "../inc.cfg"]);
        std::fs::write(co.join("inc.cfg"), "[alias]\n\tx = !sh ./evil.sh\n").unwrap();
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&co)
            .args(["config", "--list", "--show-origin", "--includes", "-z"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        let v = targets_from_config(&out.stdout, &co, None);
        for want in [".githooks", "fsmon", "inc.cfg", "evil.sh"] {
            assert!(v.contains(&co.join(want)), "{want} missing from {v:?}");
        }
    }
}
