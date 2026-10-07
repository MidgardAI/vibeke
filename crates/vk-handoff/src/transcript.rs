//! Harness transcripts: redacted on export, installed with rewritten paths on import.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{
    MAX_SIDECHAIN, Manifest, NotWritten, claude_project_dir, create_new_under, files_under,
    harness_home, resume_args, safe_relative, transcript_relative, valid_session_id,
};

pub(crate) const MAX_SIDECHAIN_FILES: usize = 1000;

/// Redact line by line. Untouched lines stay byte-for-byte; returns the number of changed lines.
pub fn redact_lines(text: &str) -> (String, usize) {
    let mut redactions = 0;
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let red = match serde_json::from_str::<Value>(line) {
            Ok(mut v) => {
                let before = v.to_string();
                vk_redact::redact_json(&mut v);
                let after = v.to_string();
                if after == before {
                    line.to_string()
                } else {
                    after
                }
            }
            Err(_) => vk_redact::redact(line).to_string(),
        };
        if red != line {
            redactions += 1;
        }
        out.push_str(&red);
        out.push('\n');
    }
    (out, redactions)
}

/// Copy the transcript at `path` into `work/transcript.jsonl`, redacted; for Claude also its
/// `<session>/` directory (subagents, sidechains) into `work/sidechain/`. Returns the path relative
/// to the harness home and the number of redacted lines, or `None` when there is nothing to carry.
pub fn export_transcript(
    harness: &str,
    path: &Path,
    work: &Path,
) -> std::io::Result<Option<(String, usize)>> {
    let Some(rel) = transcript_relative(harness, &path.to_string_lossy()) else {
        return Ok(None);
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    let (out, mut redactions) = redact_lines(&text);
    std::fs::write(work.join("transcript.jsonl"), out)?;
    if harness == "claude" {
        redactions += export_sidechain(&path.with_extension(""), &work.join("sidechain"))?;
    }
    Ok(Some((rel, redactions)))
}

/// Regular files only, never through a symlink, at most [`MAX_SIDECHAIN`] bytes in all.
fn export_sidechain(src: &Path, dest: &Path) -> std::io::Result<usize> {
    if !std::fs::symlink_metadata(src).is_ok_and(|m| m.is_dir()) {
        return Ok(0);
    }
    let mut left = MAX_SIDECHAIN;
    let mut redactions = 0;
    for rel in files_under(src, MAX_SIDECHAIN_FILES)? {
        let mut data = Vec::new();
        let read = (|| -> std::io::Result<()> {
            use std::os::unix::fs::OpenOptionsExt;
            let f = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(src.join(&rel))?;
            if !f.metadata()?.is_file() {
                return Err(std::io::Error::other("not a regular file"));
            }
            f.take(left + 1).read_to_end(&mut data)?;
            Ok(())
        })();
        if read.is_err() {
            continue;
        }
        if data.len() as u64 > left {
            break;
        }
        let Ok(text) = String::from_utf8(data) else {
            continue;
        };
        left -= text.len() as u64;
        let (out, n) = redact_lines(&text);
        redactions += n;
        std::fs::create_dir_all(dest)?;
        create_new_under(dest, &rel, 0o600)?.write_all(out.as_bytes())?;
    }
    Ok(redactions)
}

fn json_escaped(s: &str) -> String {
    let j = serde_json::to_string(s).unwrap_or_default();
    j[1..j.len() - 1].to_string()
}

/// Replace each `(from, to)` path in JSON text in one pass (a replacement is never rewritten
/// again). A path matches only as a whole: not preceded by a name character and followed by `/`,
/// `"`, `\` or the end of the text. The JSON-escaped form (`\/` for `/`) is replaced too.
pub fn rewrite_paths(text: &str, pairs: &[(&str, &str)]) -> String {
    let mut pats: Vec<(String, String)> = Vec::new();
    for (from, to) in pairs.iter().filter(|(f, _)| !f.is_empty()) {
        let (f, t) = (json_escaped(from), json_escaped(to));
        pats.push((f.replace('/', "\\/"), t.replace('/', "\\/")));
        pats.push((f, t));
    }
    // Longest first, so a cwd below the root wins over the root.
    pats.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
    let b = text.as_bytes();
    let left_ok =
        |i: usize| i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b"._-~".contains(&b[i - 1]));
    let right_ok = |j: usize| j == b.len() || matches!(b[j], b'/' | b'"' | b'\\');
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'scan: while i < text.len() {
        for (f, t) in &pats {
            if text[i..].starts_with(f.as_str()) && left_ok(i) && right_ok(i + f.len()) {
                out.push_str(t);
                i += f.len();
                continue 'scan;
            }
        }
        let c = text[i..].chars().next().unwrap_or_default();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Set every `sessionId` equal to `old` to `new`, line by line; other lines stay byte-for-byte.
pub fn rewrite_session(text: &str, old: &str, new: &str) -> String {
    fn walk(v: &mut Value, old: &str, new: &str) -> bool {
        match v {
            Value::Object(map) => {
                let mut changed = false;
                for (k, x) in map.iter_mut() {
                    if k == "sessionId" && x.as_str() == Some(old) {
                        *x = Value::String(new.into());
                        changed = true;
                    } else {
                        changed |= walk(x, old, new);
                    }
                }
                changed
            }
            Value::Array(a) => a.iter_mut().fold(false, |c, x| walk(x, old, new) | c),
            _ => false,
        }
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (body, nl) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        let changed = body
            .contains(old)
            .then(|| serde_json::from_str::<Value>(body).ok())
            .flatten()
            .and_then(|mut v| walk(&mut v, old, new).then(|| v.to_string()));
        out.push_str(changed.as_deref().unwrap_or(body));
        out.push_str(nl);
    }
    out
}

/// Set the Codex session id `old` to `new`: `payload.id` of `session_meta` lines and the `id` of
/// a legacy first line (`{id, timestamp, instructions}`). Other lines stay byte-for-byte.
pub fn rewrite_codex_session(text: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let (body, nl) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        let changed = body
            .contains(old)
            .then(|| serde_json::from_str::<Value>(body).ok())
            .flatten()
            .and_then(|mut v| {
                let meta = v.get("type").and_then(Value::as_str) == Some("session_meta");
                let legacy = i == 0 && v.get("type").is_none();
                let slot = if meta {
                    v.pointer_mut("/payload/id")
                } else if legacy {
                    v.get_mut("id")
                } else {
                    None
                }?;
                if slot.as_str() != Some(old) {
                    return None;
                }
                *slot = Value::String(new.into());
                Some(v.to_string())
            });
        out.push_str(changed.as_deref().unwrap_or(body));
        out.push_str(nl);
    }
    out
}

/// Whether a rollout for session `id` exists below `sessions` (`.../rollout-<ts>-<id>.jsonl`).
fn codex_session_exists(sessions: &Path, id: &str) -> bool {
    let suffix = format!("{id}.jsonl");
    let mut stack = vec![(sessions.to_path_buf(), 0)];
    let mut seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            seen += 1;
            if seen > 200_000 {
                // Too many to look through: assume taken; a fresh id costs nothing.
                return true;
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            match e.file_type() {
                Ok(t) if t.is_dir() && depth < 6 => stack.push((e.path(), depth + 1)),
                _ if name.ends_with(&suffix) => return true,
                _ => {}
            }
        }
    }
    false
}

/// A transcript the harness can resume from.
#[derive(Debug, Clone)]
pub struct Installed {
    /// The session id it was installed under (fresh when the original was taken).
    pub session: String,
    pub resume_args: Vec<String>,
    pub path: PathBuf,
    /// Sidechain files that could not be installed.
    pub not_written: Vec<NotWritten>,
}

/// [`install_transcript_in`] the harness's own home (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, ...).
pub fn install_transcript(
    m: &Manifest,
    work: &Path,
    new_cwd: &Path,
    new_root: &Path,
) -> std::io::Result<Option<Installed>> {
    match m.harness.as_deref().and_then(harness_home) {
        Some(home) => install_transcript_in(&home, m, work, new_cwd, new_root),
        None => Ok(None),
    }
}

/// Install the unpacked transcript below `home` where the harness resumes from, with the source
/// paths rewritten to the new worktree. A session that already exists there is never replaced:
/// the harness gets a fresh session id, rewritten in the transcript (and for Codex in the rollout's
/// file name). `None` when not resumable.
pub fn install_transcript_in(
    home: &Path,
    m: &Manifest,
    work: &Path,
    new_cwd: &Path,
    new_root: &Path,
) -> std::io::Result<Option<Installed>> {
    let src = work.join("transcript.jsonl");
    let (Some(h), Some(rel), true) = (
        m.harness.as_deref(),
        m.transcript_rel.as_deref(),
        src.exists(),
    ) else {
        return Ok(None);
    };
    let Some(session) = m.session_id.as_deref().filter(|id| valid_session_id(id)) else {
        return Ok(None);
    };
    if resume_args(h, Some(session)).is_none() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&src)?;
    if raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .any(|l| serde_json::from_str::<Value>(l).is_err())
    {
        return Err(std::io::Error::other("transcript is not JSON lines"));
    }
    let (cwd, root) = (
        new_cwd.display().to_string(),
        new_root.display().to_string(),
    );
    let pairs = [
        (m.source_cwd.as_str(), cwd.as_str()),
        (m.source_root.as_str(), root.as_str()),
    ];
    let text = rewrite_paths(&raw, &pairs);
    let file = Path::new(rel)
        .file_name()
        .ok_or_else(|| std::io::Error::other("bad transcript path"))?;
    match h {
        "claude" => {
            let dir = home.join("projects").join(claude_project_dir(new_cwd));
            let taken = |id: &str| {
                std::fs::symlink_metadata(dir.join(format!("{id}.jsonl"))).is_ok()
                    || std::fs::symlink_metadata(dir.join(id)).is_ok()
            };
            std::fs::create_dir_all(&dir)?;
            let fresh = |t: &str, id: &str| {
                if id == session {
                    t.to_string()
                } else {
                    rewrite_session(t, session, id)
                }
            };
            // The transcript file is created with `create_new`, so an import of the same session
            // running at the same time can't take it between the check and the write.
            let (id, dest) = claim_session(session, |id| {
                if taken(id) {
                    return Ok(None);
                }
                let dest = dir.join(format!("{id}.jsonl"));
                write_new(&dest, &fresh(&text, id)).map(|()| Some(dest))
            })?;
            let mut not_written = Vec::new();
            let side = work.join("sidechain");
            if std::fs::symlink_metadata(&side).is_ok_and(|m| m.is_dir()) {
                let sdir = dir.join(&id);
                for rel in files_under(&side, MAX_SIDECHAIN_FILES)? {
                    let r = (|| -> std::io::Result<()> {
                        let raw = std::fs::read_to_string(side.join(&rel))?;
                        let t = fresh(&rewrite_paths(&raw, &pairs), &id);
                        std::fs::create_dir_all(&sdir)?;
                        create_new_under(&sdir, &rel, 0o600)?.write_all(t.as_bytes())
                    })();
                    if let Err(e) = r {
                        not_written.push(NotWritten {
                            path: format!("{id}/{rel}"),
                            reason: e.to_string(),
                        });
                    }
                }
            }
            Ok(Some(Installed {
                resume_args: resume_args("claude", Some(&id)).unwrap_or_default(),
                session: id,
                path: dest,
                not_written,
            }))
        }
        "codex" => {
            // Only a session file below `sessions/`, never e.g. `config.toml`.
            let name = file.to_string_lossy();
            if !safe_relative(rel)
                || !rel.starts_with("sessions/")
                || !name.ends_with(".jsonl")
                || !name.contains(session)
            {
                return Err(std::io::Error::other("bad transcript path"));
            }
            // `codex resume <id>` finds the rollout by the id at the end of its file name
            // anywhere below `sessions/`, so an id that exists there gets a fresh one.
            let sessions = home.join("sessions");
            let (id, dest) = claim_session(session, |id| {
                if codex_session_exists(&sessions, id) {
                    return Ok(None);
                }
                let mut dest = home.join(rel);
                let text = if id == session {
                    text.clone()
                } else {
                    // The id is the last occurrence in the name (`rollout-<ts>-<id>.jsonl`).
                    let at = name.rfind(session).unwrap_or_default();
                    dest.set_file_name(format!(
                        "{}{id}{}",
                        &name[..at],
                        &name[at + session.len()..]
                    ));
                    rewrite_codex_session(&text, session, id)
                };
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                write_new(&dest, &text).map(|()| Some(dest))
            })?;
            Ok(Some(Installed {
                resume_args: resume_args("codex", Some(&id)).unwrap_or_default(),
                session: id,
                path: dest,
                not_written: Vec::new(),
            }))
        }
        _ => Ok(None),
    }
}

/// Install under `session`, else under fresh ids: `try_id` returns `None` when the id is taken
/// and creates the file with `create_new` otherwise; a file that appeared since the check
/// (`AlreadyExists`, another import of the same session) counts as taken too.
fn claim_session(
    session: &str,
    mut try_id: impl FnMut(&str) -> std::io::Result<Option<PathBuf>>,
) -> std::io::Result<(String, PathBuf)> {
    let mut id = session.to_string();
    for _ in 0..16 {
        match try_id(&id) {
            Ok(Some(dest)) => return Ok((id, dest)),
            Ok(None) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        id = new_session_id();
    }
    Err(std::io::Error::other("no free session id"))
}

fn write_new(dest: &Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dest)?
        .write_all(text.as_bytes())
}

/// A random (v4) UUID, the form Claude uses for session ids.
fn new_session_id() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = crate::hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rewrite_only_whole_paths() {
        let p = [("/a/app", "/b/wt")];
        assert_eq!(
            rewrite_paths(r#"{"cwd":"/a/app","f":"/a/app/x"}"#, &p),
            r#"{"cwd":"/b/wt","f":"/b/wt/x"}"#
        );
        // Prefix collisions on either side stay.
        let s = r#"{"f":"/a/app-old/x","g":"/x/a/app","h":"/a/apple"}"#;
        assert_eq!(rewrite_paths(s, &p), s);
        // JSON-escaped slashes and a path ending the text.
        assert_eq!(rewrite_paths(r#""\/a\/app\/y""#, &p), r#""\/b\/wt\/y""#);
        assert_eq!(rewrite_paths("/a/app", &p), "/b/wt");
        // Prose without a boundary is left alone.
        assert_eq!(rewrite_paths("cd /a/app && ls", &p), "cd /a/app && ls");
        // The destination is JSON-escaped.
        assert_eq!(
            rewrite_paths(r#""/a/app""#, &[("/a/app", "/b/q\"t")]),
            r#""/b/q\"t""#
        );
    }

    #[test]
    fn rewrite_is_one_pass_longest_first() {
        let p = [("/a/app/sub", "/a/app-h/sub"), ("/a/app", "/a/app-h")];
        assert_eq!(
            rewrite_paths(r#"["/a/app/sub/f","/a/app/g"]"#, &p),
            r#"["/a/app-h/sub/f","/a/app-h/g"]"#
        );
        // A replacement that contains the source is not rewritten again.
        let p = [("/a", "/a/a")];
        assert_eq!(rewrite_paths(r#""/a/x""#, &p), r#""/a/a/x""#);
    }

    #[test]
    fn session_ids_rewritten() {
        let t = format!(
            "{}\nnot json sess1\n{}\n",
            json!({"sessionId": "sess1", "m": {"sessionId": "sess1"}, "x": "sess1"}),
            json!({"sessionId": "other"})
        );
        let out = rewrite_session(&t, "sess1", "new");
        let first: Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
        assert_eq!(first["sessionId"], "new");
        assert_eq!(first["m"]["sessionId"], "new");
        assert_eq!(first["x"], "sess1", "only sessionId fields");
        assert_eq!(out.lines().nth(1), Some("not json sess1"));
        assert!(out.ends_with(&format!("{}\n", json!({"sessionId": "other"}))));
    }

    fn claude_work(root: &Path) -> (PathBuf, Manifest) {
        let work = root.join("w");
        std::fs::create_dir_all(work.join("sidechain/subagents")).unwrap();
        std::fs::write(
            work.join("transcript.jsonl"),
            format!(
                "{}\n",
                json!({"sessionId": "sess1", "cwd": "/src/app", "f": "/src-old/x"})
            ),
        )
        .unwrap();
        std::fs::write(
            work.join("sidechain/subagents/agent-1.jsonl"),
            format!(
                "{}\n",
                json!({"sessionId": "sess1", "cwd": "/src/app", "isSidechain": true})
            ),
        )
        .unwrap();
        let m = Manifest {
            harness: Some("claude".into()),
            session_id: Some("sess1".into()),
            transcript_rel: Some("projects/-src-app/sess1.jsonl".into()),
            source_cwd: "/src/app".into(),
            source_root: "/src".into(),
            ..Default::default()
        };
        (work, m)
    }

    #[test]
    fn claude_sidechain_installed_next_to_transcript() {
        let t = tempfile::tempdir().unwrap();
        let (work, m) = claude_work(t.path());
        let home = t.path().join("claude");
        let (cwd, root) = (Path::new("/dst/wt/app"), Path::new("/dst/wt"));
        let got = install_transcript_in(&home, &m, &work, cwd, root)
            .unwrap()
            .unwrap();
        assert_eq!(got.session, "sess1");
        assert_eq!(got.resume_args, vec!["--resume", "sess1"]);
        assert!(got.not_written.is_empty());
        let dir = home.join("projects").join(claude_project_dir(cwd));
        let main = std::fs::read_to_string(dir.join("sess1.jsonl")).unwrap();
        assert!(main.contains(r#""cwd":"/dst/wt/app""#));
        assert!(
            main.contains(r#""f":"/src-old/x""#),
            "prefix collision kept"
        );
        let side = std::fs::read_to_string(dir.join("sess1/subagents/agent-1.jsonl")).unwrap();
        assert!(side.contains(r#""cwd":"/dst/wt/app""#));
    }

    #[test]
    fn claude_session_collision_gets_fresh_id() {
        let t = tempfile::tempdir().unwrap();
        let (work, m) = claude_work(t.path());
        let home = t.path().join("claude");
        let (cwd, root) = (Path::new("/dst/wt/app"), Path::new("/dst/wt"));
        install_transcript_in(&home, &m, &work, cwd, root)
            .unwrap()
            .unwrap();
        let again = install_transcript_in(&home, &m, &work, cwd, root)
            .unwrap()
            .expect("still resumable");
        assert_ne!(again.session, "sess1");
        assert!(valid_session_id(&again.session));
        assert_eq!(
            again.resume_args,
            vec!["--resume".to_string(), again.session.clone()]
        );
        let dir = home.join("projects").join(claude_project_dir(cwd));
        assert_eq!(again.path, dir.join(format!("{}.jsonl", again.session)));
        let text = std::fs::read_to_string(&again.path).unwrap();
        let v: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(v["sessionId"], again.session.as_str());
        let side =
            std::fs::read_to_string(dir.join(&again.session).join("subagents/agent-1.jsonl"))
                .unwrap();
        assert!(side.contains(&again.session));
        // The original is untouched.
        assert!(
            std::fs::read_to_string(dir.join("sess1.jsonl"))
                .unwrap()
                .contains(r#""sessionId":"sess1""#)
        );
    }

    #[test]
    fn codex_transcript_stays_in_sessions() {
        let t = tempfile::tempdir().unwrap();
        let work = t.path().join("w");
        std::fs::create_dir(&work).unwrap();
        let s1 = "0199aaaa-bbbb-4ccc-8ddd-eeeeffff0001";
        std::fs::write(
            work.join("transcript.jsonl"),
            format!(
                "{}\n{}\n",
                json!({"type": "session_meta", "payload": {"id": s1, "cwd": "/src/app"}}),
                json!({"type": "response_item", "payload": {"text": s1}})
            ),
        )
        .unwrap();
        let home = t.path().join("codex");
        let mut m = Manifest {
            harness: Some("codex".into()),
            session_id: Some(s1.into()),
            ..Default::default()
        };
        m.transcript_rel = Some("config.toml".into());
        assert!(install_transcript_in(&home, &m, &work, t.path(), t.path()).is_err());
        m.transcript_rel = Some("sessions/../config.toml".into());
        assert!(install_transcript_in(&home, &m, &work, t.path(), t.path()).is_err());
        m.transcript_rel = Some(format!(
            "sessions/2026/10/06/rollout-2026-10-06T08-00-00-{s1}.jsonl"
        ));
        let first = install_transcript_in(&home, &m, &work, t.path(), t.path())
            .unwrap()
            .unwrap();
        assert_eq!(first.session, s1);
        assert_eq!(
            first.path,
            home.join(format!(
                "sessions/2026/10/06/rollout-2026-10-06T08-00-00-{s1}.jsonl"
            ))
        );
        assert_eq!(first.resume_args, vec!["resume", s1]);

        // The session exists on this host (on another day, too): a fresh id, in the file name
        // and the session_meta line, so `codex resume <id>` can only find the handed-off copy.
        std::fs::remove_file(&first.path).unwrap();
        let other_day = home.join(format!(
            "sessions/2026/10/01/rollout-2026-10-01T08-00-00-{s1}.jsonl"
        ));
        std::fs::create_dir_all(other_day.parent().unwrap()).unwrap();
        std::fs::write(&other_day, "original\n").unwrap();
        let second = install_transcript_in(&home, &m, &work, t.path(), t.path())
            .unwrap()
            .unwrap();
        assert_ne!(second.session, s1);
        assert!(valid_session_id(&second.session));
        assert_eq!(
            second.path,
            home.join(format!(
                "sessions/2026/10/06/rollout-2026-10-06T08-00-00-{}.jsonl",
                second.session
            ))
        );
        assert_eq!(
            second.resume_args,
            vec!["resume".to_string(), second.session.clone()]
        );
        let text = std::fs::read_to_string(&second.path).unwrap();
        let mut lines = text
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap());
        assert_eq!(
            lines.next().unwrap()["payload"]["id"],
            second.session.as_str()
        );
        assert_eq!(
            lines.next().unwrap()["payload"]["text"],
            s1,
            "only the session id"
        );
        assert_eq!(std::fs::read_to_string(&other_day).unwrap(), "original\n");
    }

    #[test]
    fn a_session_file_that_appears_after_the_check_gets_a_fresh_id() {
        let t = tempfile::tempdir().unwrap();
        let mut n = 0;
        // The first id's file appears between the check and the create (another import).
        let (id, dest) = claim_session("s1", |id| {
            n += 1;
            let dest = t.path().join(format!("{id}.jsonl"));
            if n == 1 {
                std::fs::write(&dest, "theirs").unwrap();
            }
            write_new(&dest, "mine").map(|()| Some(dest))
        })
        .unwrap();
        assert_ne!(id, "s1");
        assert!(valid_session_id(&id));
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "mine");
        assert_eq!(
            std::fs::read_to_string(t.path().join("s1.jsonl")).unwrap(),
            "theirs"
        );
        // Other errors are errors.
        let e = claim_session("s1", |_| Err(std::io::Error::other("disk full"))).unwrap_err();
        assert_eq!(e.to_string(), "disk full");
        assert!(claim_session("s1", |_| Ok(None)).is_err());
    }

    #[test]
    fn concurrent_installs_of_one_session_all_resume() {
        let t = tempfile::tempdir().unwrap();
        let (cwd, root) = (Path::new("/dst/wt/app"), Path::new("/dst/wt"));
        let (cwork, cm) = claude_work(t.path());
        let chome = t.path().join("claude");
        let xwork = t.path().join("x");
        std::fs::create_dir(&xwork).unwrap();
        let s1 = "0199aaaa-bbbb-4ccc-8ddd-eeeeffff0002";
        std::fs::write(
            xwork.join("transcript.jsonl"),
            format!(
                "{}\n",
                json!({"type": "session_meta", "payload": {"id": s1}})
            ),
        )
        .unwrap();
        let xm = Manifest {
            harness: Some("codex".into()),
            session_id: Some(s1.into()),
            transcript_rel: Some(format!("sessions/2026/10/07/rollout-t-{s1}.jsonl")),
            ..Default::default()
        };
        let xhome = t.path().join("codex");
        for (home, m, work) in [(&chome, &cm, &cwork), (&xhome, &xm, &xwork)] {
            let got: Vec<Installed> = std::thread::scope(|sc| {
                let hs: Vec<_> = (0..8)
                    .map(|_| sc.spawn(|| install_transcript_in(home, m, work, cwd, root)))
                    .collect();
                hs.into_iter()
                    .map(|h| h.join().unwrap().unwrap().expect("resumable"))
                    .collect()
            });
            let mut ids: Vec<&str> = got.iter().map(|i| i.session.as_str()).collect();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), 8, "every import has its own session: {ids:?}");
            for i in &got {
                assert!(i.path.is_file());
                assert_eq!(i.resume_args.last(), Some(&i.session));
            }
        }
    }

    #[test]
    fn codex_legacy_first_line_id_rewritten() {
        let t = format!(
            "{}\n{}\n",
            json!({"id": "s1", "timestamp": "x", "instructions": null}),
            json!({"id": "s1", "type": "message"})
        );
        let out = rewrite_codex_session(&t, "s1", "s2");
        let mut lines = out.lines();
        assert_eq!(
            serde_json::from_str::<Value>(lines.next().unwrap()).unwrap()["id"],
            "s2"
        );
        assert_eq!(lines.next(), t.lines().nth(1));
    }

    #[test]
    fn export_carries_redacted_sidechain() {
        let t = tempfile::tempdir().unwrap();
        let proj = t.path().join("claude/projects/-src-app");
        std::fs::create_dir_all(proj.join("sess1/subagents")).unwrap();
        std::fs::write(proj.join("sess1.jsonl"), "{\"m\":\"hi\"}\n").unwrap();
        std::fs::write(
            proj.join("sess1/subagents/agent-1.jsonl"),
            format!(
                "{}\n",
                json!({"m": "export OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz0123456789"})
            ),
        )
        .unwrap();
        let secret = t.path().join("secret.txt");
        std::fs::write(&secret, "outside").unwrap();
        std::os::unix::fs::symlink(&secret, proj.join("sess1/subagents/link.jsonl")).unwrap();
        let work = t.path().join("w");
        std::fs::create_dir(&work).unwrap();
        let (rel, n) = export_transcript("claude", &proj.join("sess1.jsonl"), &work)
            .unwrap()
            .unwrap();
        assert_eq!(rel, "projects/-src-app/sess1.jsonl");
        assert_eq!(n, 1);
        let side = std::fs::read_to_string(work.join("sidechain/subagents/agent-1.jsonl")).unwrap();
        assert!(!side.contains("sk-abcdefghijklmnopqrstuvwxyz"));
        assert!(!work.join("sidechain/subagents/link.jsonl").exists());
    }
}
