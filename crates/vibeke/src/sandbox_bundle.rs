//! In-box halves of a cloud move (spec 17 §7). They run inside a cloud box (or anywhere: they
//! are plain subcommands without a server):
//!
//! - `vibeke sandbox export-bundle --cwd DIR [--harness H] [--session S] [--transcript PATH]
//!   [--resume-arg A]... [--full] [--source-host NAME]` exports the work in `DIR`'s repository
//!   with `vk_handoff::export` and writes the bundle (tar.zst) to stdout.
//! - `vibeke sandbox import-bundle --bundle PATH --workspace DIR` imports a bundle in place into
//!   the checkout at `DIR` (`vk_handoff::import_in_place`; the transcript goes below `$HOME`'s
//!   harness directory) and prints `{cwd, resume_argv, ...}` as JSON on stdout.
//!
//! Errors go to stderr as `vibeke sandbox <cmd>: <kind>: <message>` with exit code 1; usage
//! errors exit 2.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;
use vk_cli::{EXIT_API, EXIT_OK, EXIT_USAGE};

const EXPORT_USAGE: &str = "usage: vibeke sandbox export-bundle --cwd DIR [--harness H] [--session S] [--transcript PATH] [--resume-arg A]... [--full] [--source-host NAME]";
const IMPORT_USAGE: &str = "usage: vibeke sandbox import-bundle --bundle PATH --workspace DIR";

#[derive(Debug, Default, PartialEq, Eq)]
struct ExportArgs {
    cwd: PathBuf,
    harness: Option<String>,
    session: Option<String>,
    transcript: Option<PathBuf>,
    resume_args: Vec<String>,
    full: bool,
    source_host: Option<String>,
}

fn parse_export(args: &[String]) -> Result<ExportArgs, String> {
    let mut out = ExportArgs::default();
    let mut cwd = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--cwd" => cwd = Some(PathBuf::from(val("--cwd")?)),
            "--harness" => out.harness = Some(val("--harness")?),
            "--session" => out.session = Some(val("--session")?),
            "--transcript" => out.transcript = Some(PathBuf::from(val("--transcript")?)),
            "--resume-arg" => out.resume_args.push(val("--resume-arg")?),
            "--source-host" => out.source_host = Some(val("--source-host")?),
            "--full" => out.full = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    out.cwd = cwd.ok_or("--cwd is required")?;
    Ok(out)
}

fn parse_import(args: &[String]) -> Result<(PathBuf, PathBuf), String> {
    let (mut bundle, mut ws) = (None, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.as_str() {
            "--bundle" => bundle = Some(PathBuf::from(val("--bundle")?)),
            "--workspace" => ws = Some(PathBuf::from(val("--workspace")?)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok((
        bundle.ok_or("--bundle is required")?,
        ws.ok_or("--workspace is required")?,
    ))
}

fn fail(cmd: &str, e: impl std::fmt::Display) -> i32 {
    eprintln!("vibeke sandbox {cmd}: {e}");
    EXIT_API
}

/// This machine's name for the manifest (`--source-host`, else the hostname).
fn host_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "cloud box".into())
}

/// The harness transcript of `session` when the caller did not name one: Claude keeps it at
/// `projects/<cwd as dir name>/<session>.jsonl` (any project directory as a fallback), Codex at
/// `sessions/**/rollout-…-<session>.jsonl`.
fn find_transcript(harness: &str, session: &str, cwd: &Path) -> Option<PathBuf> {
    if !vk_handoff::valid_session_id(session) {
        return None;
    }
    let home = vk_handoff::harness_home(harness)?;
    match harness {
        "claude" => {
            let projects = home.join("projects");
            let direct = projects
                .join(vk_handoff::claude_project_dir(cwd))
                .join(format!("{session}.jsonl"));
            if direct.is_file() {
                return Some(direct);
            }
            std::fs::read_dir(&projects)
                .ok()?
                .flatten()
                .map(|e| e.path().join(format!("{session}.jsonl")))
                .find(|p| p.is_file())
        }
        "codex" => {
            let suffix = format!("{session}.jsonl");
            let mut stack = vec![(home.join("sessions"), 0usize)];
            while let Some((dir, depth)) = stack.pop() {
                for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                    let Ok(ft) = e.file_type() else { continue };
                    let p = e.path();
                    if ft.is_dir() && depth < 4 {
                        stack.push((p, depth + 1));
                    } else if ft.is_file() && e.file_name().to_string_lossy().ends_with(&suffix) {
                        return Some(p);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// `vibeke sandbox export-bundle`.
pub async fn export_bundle(args: &[String]) -> i32 {
    let a = match parse_export(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{EXPORT_USAGE}");
            return EXIT_USAGE;
        }
    };
    let cwd = a.cwd.canonicalize().unwrap_or_else(|_| a.cwd.clone());
    let transcript =
        a.transcript
            .clone()
            .or_else(|| match (a.harness.as_deref(), a.session.as_deref()) {
                (Some(h), Some(s)) => find_transcript(h, s, &cwd),
                _ => None,
            });
    let resume_args = if a.resume_args.is_empty() {
        a.harness
            .as_deref()
            .and_then(|h| vk_handoff::resume_args(h, a.session.as_deref()))
            .unwrap_or_default()
    } else {
        a.resume_args.clone()
    };
    let tmp = match tempfile::Builder::new().prefix("vibeke-export-").tempdir() {
        Ok(t) => t,
        Err(e) => return fail("export-bundle", e),
    };
    let out = tmp.path().join("bundle.tar.zst");
    let input = vk_handoff::ExportInput {
        cwd,
        pin: None,
        harness: a.harness,
        session_id: a.session,
        transcript,
        resume_args,
        last_message: None,
        source_host: a.source_host.unwrap_or_else(host_name),
        source_job: None,
        full: a.full,
    };
    if let Err(e) = vk_handoff::export(&input, &out).await {
        return fail("export-bundle", e);
    }
    let copied = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::open(&out)?;
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        std::io::copy(&mut f, &mut lock)?;
        lock.flush()
    })();
    match copied {
        Ok(()) => EXIT_OK,
        Err(e) => fail("export-bundle", e),
    }
}

/// `vibeke sandbox import-bundle`.
pub async fn import_bundle(args: &[String]) -> i32 {
    let (bundle, ws) = match parse_import(args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}\n{IMPORT_USAGE}");
            return EXIT_USAGE;
        }
    };
    let root = match vk_handoff::git_line(&ws, &["rev-parse", "--show-toplevel"]).await {
        Some(r) => PathBuf::from(r),
        None => {
            return fail(
                "import-bundle",
                format!("not_found: {} is not a git checkout", ws.display()),
            );
        }
    };
    let tmp = match tempfile::Builder::new().prefix("vibeke-import-").tempdir() {
        Ok(t) => t,
        Err(e) => return fail("import-bundle", e),
    };
    let work = tmp.path().to_path_buf();
    let (b2, w2) = (bundle.clone(), work.clone());
    let m = match tokio::task::spawn_blocking(move || vk_handoff::unpack(&b2, &w2)).await {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => return fail("import-bundle", format!("invalid_params: bad bundle: {e}")),
        Err(e) => return fail("import-bundle", e),
    };
    let imported = match vk_handoff::import_in_place(&work, &m, &root).await {
        Ok(i) => i,
        Err(e) => return fail("import-bundle", e),
    };
    drop(tmp);
    // The uploaded bundle is consumed.
    let _ = std::fs::remove_file(&bundle);
    let harness = m
        .harness
        .as_deref()
        .filter(|h| vk_handoff::known_harness(h));
    let resume_argv = harness.map(|h| {
        let mut v = vec![h.to_string()];
        if imported.resumed {
            v.extend(imported.resume_args.clone().unwrap_or_default());
        }
        v
    });
    let out = json!({
        "cwd": imported.cwd,
        "resume_argv": resume_argv,
        "resumed": imported.resumed,
        "harness": harness,
        "branch": imported.branch,
        "head": m.head,
        "not_written": imported.not_written,
        "skipped": m.skipped,
    });
    println!("{out}");
    EXIT_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn export_flags() {
        let a = parse_export(&v(&[
            "--cwd",
            "/workspace/app",
            "--harness",
            "claude",
            "--session",
            "s1",
            "--resume-arg",
            "--resume",
            "--resume-arg",
            "s1",
            "--full",
        ]))
        .unwrap();
        assert_eq!(a.cwd, PathBuf::from("/workspace/app"));
        assert_eq!(a.harness.as_deref(), Some("claude"));
        assert_eq!(a.resume_args, v(&["--resume", "s1"]));
        assert!(a.full);
        assert!(parse_export(&v(&["--harness", "claude"])).is_err());
        assert!(parse_export(&v(&["--cwd"])).is_err());
        assert!(parse_export(&v(&["--cwd", "/x", "--bogus"])).is_err());
    }

    #[test]
    fn import_flags() {
        let (b, w) = parse_import(&v(&["--bundle", "/b.tar.zst", "--workspace", "/w"])).unwrap();
        assert_eq!(b, PathBuf::from("/b.tar.zst"));
        assert_eq!(w, PathBuf::from("/w"));
        assert!(parse_import(&v(&["--bundle", "/b"])).is_err());
    }
}
