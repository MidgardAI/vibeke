//! The in-box halves of a cloud move (spec 17 §7) as plain subcommands on the host:
//! `vibeke sandbox export-bundle` writes a bundle to stdout and `vibeke sandbox import-bundle`
//! imports it in place into another checkout, transcript included.

use std::path::Path;
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn vibeke(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_vibeke"))
        .args(args)
        .env("HOME", home)
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn export_bundle_then_import_bundle_round_trip() {
    let t = tempfile::Builder::new()
        .prefix("vkcm")
        .tempdir_in("/tmp")
        .unwrap();
    let root = t.path().canonicalize().unwrap();

    // The source: committed work on a branch, an uncommitted change and an untracked file.
    let src = root.join("src");
    std::fs::create_dir_all(src.join("app")).unwrap();
    git(&src, &["init", "-q", "-b", "main"]);
    std::fs::write(src.join("app/a.txt"), "one\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-qm", "base"]);
    let base = git(&src, &["rev-parse", "HEAD"]);

    // The destination: a clone at the base commit on the same branch (as a fresh box is).
    let dst = root.join("dst");
    git(
        &root,
        &["clone", "-q", src.to_str().unwrap(), dst.to_str().unwrap()],
    );
    git(&dst, &["checkout", "-q", "-B", "feature", &base]);
    git(&dst, &["remote", "remove", "origin"]);

    git(&src, &["checkout", "-qb", "feature"]);
    std::fs::write(src.join("app/b.txt"), "two\n").unwrap();
    git(&src, &["add", "-A"]);
    git(&src, &["commit", "-qm", "feature work"]);
    let head = git(&src, &["rev-parse", "HEAD"]);
    std::fs::write(src.join("app/a.txt"), "one\nchanged\n").unwrap();
    std::fs::write(src.join("app/notes.md"), "notes\n").unwrap();

    // A Claude transcript for the session, where Claude keeps it for the source cwd.
    let src_home = root.join("home-src");
    let session = "0b5c0d7e-1111-4222-8333-444455556666";
    let cwd = src.join("app");
    let proj = src_home
        .join(".claude/projects")
        .join(vk_handoff::claude_project_dir(&cwd));
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(
        proj.join(format!("{session}.jsonl")),
        format!(
            "{{\"type\":\"user\",\"cwd\":\"{}\",\"sessionId\":\"{session}\"}}\n",
            cwd.display()
        ),
    )
    .unwrap();

    let out = vibeke(
        &src_home,
        &[
            "sandbox",
            "export-bundle",
            "--cwd",
            cwd.to_str().unwrap(),
            "--harness",
            "claude",
            "--session",
            session,
            "--source-host",
            "box-a",
        ],
    );
    assert!(
        out.status.success(),
        "export-bundle: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.stdout.is_empty());
    let bundle = root.join("in.tar.zst");
    std::fs::write(&bundle, &out.stdout).unwrap();

    let dst_home = root.join("home-dst");
    std::fs::create_dir_all(&dst_home).unwrap();
    let out = vibeke(
        &dst_home,
        &[
            "sandbox",
            "import-bundle",
            "--bundle",
            bundle.to_str().unwrap(),
            "--workspace",
            dst.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "import-bundle: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let dst = dst.canonicalize().unwrap();
    assert_eq!(v["cwd"], dst.join("app").display().to_string());
    assert_eq!(v["head"], head.as_str());
    assert_eq!(v["resumed"], true);
    let argv: Vec<String> = serde_json::from_value(v["resume_argv"].clone()).unwrap();
    assert_eq!(argv, ["claude", "--resume", session]);

    // The work arrived in place: commit, uncommitted change and untracked file.
    assert_eq!(git(&dst, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&dst, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature");
    assert_eq!(
        std::fs::read_to_string(dst.join("app/a.txt")).unwrap(),
        "one\nchanged\n"
    );
    assert_eq!(
        std::fs::read_to_string(dst.join("app/b.txt")).unwrap(),
        "two\n"
    );
    assert_eq!(
        std::fs::read_to_string(dst.join("app/notes.md")).unwrap(),
        "notes\n"
    );
    // The transcript is installed for the new cwd, with the paths rewritten.
    let installed = dst_home
        .join(".claude/projects")
        .join(vk_handoff::claude_project_dir(&dst.join("app")))
        .join(format!("{session}.jsonl"));
    let text = std::fs::read_to_string(&installed).unwrap();
    assert!(
        text.contains(&dst.join("app").display().to_string()),
        "{text}"
    );
    // The uploaded bundle is consumed.
    assert!(!bundle.exists());

    // A broken bundle fails with the API exit code.
    std::fs::write(&bundle, b"not a bundle").unwrap();
    let out = vibeke(
        &dst_home,
        &[
            "sandbox",
            "import-bundle",
            "--bundle",
            bundle.to_str().unwrap(),
            "--workspace",
            dst.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(1));
}
