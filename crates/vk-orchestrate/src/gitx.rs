//! A small, hardened `git` runner for this crate.
//!
//! Every command goes through [`vk_tasks::safety_args`], so a checkout a contained process can
//! write to (13 §6) is never asked to run its own configured programs, and prompts are off.

use crate::{Error, Result};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Raw result of a git run that is allowed to fail.
#[derive(Debug, Clone)]
pub struct Out {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Out {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout)
            .trim_end_matches(['\n', '\r'])
            .to_string()
    }
}

fn build(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<Command> {
    let mut c = Command::new("git");
    c.arg("-C")
        .arg(dir)
        .args(vk_tasks::safety_args(dir).map_err(|e| Error::Refused(e.to_string()))?)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("LC_ALL", "C");
    for (k, v) in envs {
        c.env(k, v);
    }
    Ok(c)
}

/// Run git; never an error for a non-zero exit.
pub fn run_raw(
    dir: &Path,
    args: &[&str],
    input: Option<&[u8]>,
    envs: &[(&str, &str)],
) -> Result<Out> {
    let mut c = build(dir, args, envs)?;
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    c.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = c.spawn()?;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        let data = data.to_vec();
        // Written from a thread so a large patch cannot deadlock against a full stdout pipe.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
        });
    }
    let o = child.wait_with_output()?;
    Ok(Out {
        ok: o.status.success(),
        code: o.status.code(),
        stdout: o.stdout,
        stderr: String::from_utf8_lossy(&o.stderr).trim().to_string(),
    })
}

fn fail(args: &[&str], o: &Out) -> Error {
    Error::Git {
        args: args.join(" "),
        code: o.code,
        stderr: o.stderr.clone(),
    }
}

/// Run git and return stdout (trailing newlines trimmed); non-zero exit is an error.
pub fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let o = run_raw(dir, args, None, &[])?;
    if o.ok {
        Ok(o.text())
    } else {
        Err(fail(args, &o))
    }
}

/// Run git and return raw stdout bytes (patches, NUL-separated lists).
pub fn run_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let o = run_raw(dir, args, None, &[])?;
    if o.ok {
        Ok(o.stdout)
    } else {
        Err(fail(args, &o))
    }
}

/// Run git with `input` on stdin; non-zero exit is an error.
pub fn run_in(dir: &Path, args: &[&str], input: &[u8]) -> Result<String> {
    let o = run_raw(dir, args, Some(input), &[])?;
    if o.ok {
        Ok(o.text())
    } else {
        Err(fail(args, &o))
    }
}

/// Run git with extra environment; non-zero exit is an error.
pub fn run_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<String> {
    let o = run_raw(dir, args, None, envs)?;
    if o.ok {
        Ok(o.text())
    } else {
        Err(fail(args, &o))
    }
}

/// `true` when the command exits 0.
pub fn ok(dir: &Path, args: &[&str]) -> bool {
    run_raw(dir, args, None, &[]).map(|o| o.ok).unwrap_or(false)
}

/// Resolve `rev` to a full commit id.
pub fn rev_parse(dir: &Path, rev: &str) -> Result<String> {
    run(
        dir,
        &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")],
    )
}

/// NUL-separated output split into non-empty strings.
pub fn split_nul(b: &[u8]) -> Vec<String> {
    b.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

#[cfg(test)]
pub mod testutil {
    //! Temporary repositories for the unit tests of this crate.
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub struct Repo {
        pub dir: tempfile::TempDir,
        pub root: PathBuf,
    }

    pub fn sh(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    pub fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// A repo on branch `main` with identity configured (so Vibeke's own commits work) and
    /// the given committed files.
    pub fn repo(files: &[(&str, &str)]) -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        sh(&root, &["init", "-q", "-b", "main"]);
        sh(&root, &["config", "user.name", "T"]);
        sh(&root, &["config", "user.email", "t@example.invalid"]);
        sh(&root, &["config", "commit.gpgsign", "false"]);
        sh(&root, &["config", "core.hooksPath", "/dev/null"]);
        for (p, b) in files {
            write(&root, p, b);
        }
        sh(&root, &["add", "-A"]);
        sh(&root, &["commit", "-q", "-m", "base"]);
        Repo { dir, root }
    }

    /// Add a worktree for a new branch off `main` next to the repo.
    pub fn worktree(r: &Repo, branch: &str) -> PathBuf {
        let p = r
            .root
            .parent()
            .unwrap()
            .join(format!("wt-{}", branch.replace('/', "_")));
        sh(
            &r.root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                branch,
                p.to_str().unwrap(),
                "main",
            ],
        );
        p.canonicalize().unwrap()
    }

    pub fn commit_all(dir: &Path, msg: &str) {
        sh(dir, &["add", "-A"]);
        sh(dir, &["commit", "-q", "-m", msg]);
    }
}
