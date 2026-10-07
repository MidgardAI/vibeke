//! Handoff bundles (spec 16 §15.2): the pure parts shared by every host that exports or imports
//! an agent's work. Export and transport live with the caller; this crate packs, unpacks and runs
//! the repository-side import as one transaction.
//!
//! The bundle is a zstd-compressed tar: `manifest.json`, `repo.bundle` (optional), `changes.patch`,
//! `untracked/<path>`, `transcript.jsonl` (optional) and `sidechain/<path>` (a Claude session's
//! `<session>/` directory, optional).

mod git;
mod import;
mod transcript;

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use git::{git, git_line, set_child_umask};
pub use import::{Imported, NotWritten, import, verify};
pub use transcript::{
    Installed, export_transcript, install_transcript, install_transcript_in, redact_lines,
    rewrite_paths, rewrite_session,
};

pub const MAX_BUNDLE: u64 = 200 * 1024 * 1024;
pub const MAX_UNTRACKED: u64 = 5 * 1024 * 1024;
/// All sidechain files of one transcript together.
pub const MAX_SIDECHAIN: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Manifest {
    pub v: u32,
    pub source_host: String,
    pub repo_name: String,
    pub origin: Option<String>,
    pub branch: Option<String>,
    pub head: String,
    /// `thin` | `full` | `none` (HEAD already on a remote).
    pub bundle: String,
    /// Working directory relative to the repository root.
    pub cwd_rel: String,
    pub source_cwd: String,
    pub source_root: String,
    pub harness: Option<String>,
    pub session_id: Option<String>,
    /// `resume_argv` without the program name.
    pub resume_args: Vec<String>,
    /// Path of the transcript relative to the harness home (`projects/...` or `sessions/...`).
    pub transcript_rel: Option<String>,
    pub last_message: Option<String>,
    pub untracked: Vec<String>,
    pub skipped: Vec<Skipped>,
    pub redactions: usize,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

/// An error with an API kind (`conflict`, `timeout`, `invalid_params`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub kind: &'static str,
    pub message: String,
}

impl Error {
    pub fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Error {
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

pub fn secret_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    name == ".env"
        || name.starts_with(".env.")
        || [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks"]
            .iter()
            .any(|e| name.ends_with(e))
        || ["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"]
            .iter()
            .any(|k| name.starts_with(k))
        || matches!(
            name.as_str(),
            "credentials"
                | "credentials.json"
                | "auth.json"
                | ".netrc"
                | ".npmrc"
                | ".pypirc"
                | ".git-credentials"
        )
}

pub fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// Regular file below `root`, no symlink at any component.
pub fn regular_under(root: &Path, rel: &str) -> Option<std::fs::Metadata> {
    let mut p = root.to_path_buf();
    for c in Path::new(rel).components() {
        p.push(c);
        let md = std::fs::symlink_metadata(&p).ok()?;
        if md.file_type().is_symlink() {
            return None;
        }
    }
    std::fs::symlink_metadata(&p).ok().filter(|m| m.is_file())
}

/// Regular files below `dir` (relative, sorted), never following a symlink. Stops at `max` files.
pub(crate) fn files_under(dir: &Path, max: usize) -> std::io::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    'walk: while let Some(rel) = stack.pop() {
        for e in std::fs::read_dir(dir.join(&rel))? {
            let e = e?;
            let name = e.file_name().to_string_lossy().to_string();
            let r = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let ft = e.file_type()?;
            if ft.is_dir() {
                stack.push(r);
            } else if ft.is_file() && safe_relative(&r) {
                if out.len() >= max {
                    break 'walk;
                }
                out.push(r);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Open `root/rel` for writing without following symlinks in any existing component and without
/// replacing anything; missing directories are created.
pub(crate) fn create_new_under(
    root: &Path,
    rel: &str,
    mode: u32,
) -> std::io::Result<std::fs::File> {
    let mut p = root.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for d in &parts[..parts.len() - 1] {
        p.push(d);
        match std::fs::symlink_metadata(&p) {
            Ok(md) if md.file_type().is_symlink() || !md.is_dir() => {
                return Err(std::io::Error::other("path crosses a symlink or file"));
            }
            Ok(_) => {}
            Err(_) => {
                std::fs::create_dir(&p)?;
                user_mode(&p, 0o777)?;
            }
        }
    }
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join(rel))?;
    if let Some(m) = git::child_umask() {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(mode & !m))?;
    }
    Ok(f)
}

/// Give a directory this crate just created the user's mode bits (see [`set_child_umask`]).
fn user_mode(p: &Path, mode: u32) -> std::io::Result<()> {
    if let Some(m) = git::child_umask() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode & !m))?;
    }
    Ok(())
}

/// Create `root/rel` from `src` without following symlinks in any existing component and without
/// replacing anything.
pub fn write_new_file(root: &Path, rel: &str, src: &Path) -> std::io::Result<()> {
    if !safe_relative(rel) {
        return Err(std::io::Error::other("unsafe path"));
    }
    let mut out = create_new_under(root, rel, 0o666)?;
    std::io::copy(&mut std::fs::File::open(src)?, &mut out)?;
    Ok(())
}

/// Path of a transcript relative to its harness home (`projects/...` or `sessions/...`).
pub fn transcript_relative(harness: &str, path: &str) -> Option<String> {
    let marker = match harness {
        "claude" => "/projects/",
        "codex" => "/sessions/",
        _ => return None,
    };
    path.rfind(marker).map(|i| path[i + 1..].to_string())
}

/// Write the bundle at `out` (must not exist) from the export work dir and the source repository.
pub fn pack(
    out: &Path,
    work: &Path,
    root: &Path,
    m: &Manifest,
    bundle: bool,
) -> std::io::Result<()> {
    let f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(out)?;
    let enc = zstd::Encoder::new(f, 3)?;
    let mut tar = tar::Builder::new(enc);
    tar.follow_symlinks(false);
    let add_bytes = |tar: &mut tar::Builder<_>, name: &str, data: &[u8]| -> std::io::Result<()> {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        tar.append_data(&mut h, name, data)
    };
    add_bytes(&mut tar, "manifest.json", &serde_json::to_vec_pretty(m)?)?;
    if bundle {
        tar.append_path_with_name(work.join("repo.bundle"), "repo.bundle")?;
    }
    tar.append_path_with_name(work.join("changes.patch"), "changes.patch")?;
    if work.join("transcript.jsonl").exists() {
        tar.append_path_with_name(work.join("transcript.jsonl"), "transcript.jsonl")?;
    }
    let side = work.join("sidechain");
    if std::fs::symlink_metadata(&side).is_ok_and(|m| m.is_dir()) {
        for rel in files_under(&side, transcript::MAX_SIDECHAIN_FILES)? {
            let data = std::fs::read(side.join(&rel))?;
            add_bytes(&mut tar, &format!("sidechain/{rel}"), &data)?;
        }
    }
    for rel in &m.untracked {
        let mut data = Vec::new();
        {
            use std::os::unix::fs::OpenOptionsExt;
            let f = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(root.join(rel))?;
            if !f.metadata()?.is_file() {
                return Err(std::io::Error::other(format!(
                    "{rel} is not a regular file"
                )));
            }
            f
        }
        .take(MAX_UNTRACKED + 1)
        .read_to_end(&mut data)?;
        add_bytes(&mut tar, &format!("untracked/{rel}"), &data)?;
    }
    tar.into_inner()?.finish()?.sync_all()
}

/// Caps the decoded stream as a whole, so tar metadata (PAX sizes, long names) can't expand it.
struct Budget<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Budget<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Err(std::io::Error::other("bundle expands too much"));
        }
        let n = buf.len().min(self.left.min(usize::MAX as u64) as usize);
        let r = self.inner.read(&mut buf[..n])?;
        self.left -= r as u64;
        Ok(r)
    }
}

/// Unpack into the empty directory `out` (regular files only, relative paths only).
pub fn unpack(bundle: &Path, out: &Path) -> std::io::Result<Manifest> {
    let mut dec = zstd::Decoder::new(std::fs::File::open(bundle)?)?;
    dec.window_log_max(27)?; // ≤ 128 MiB decoder window
    let mut ar = tar::Archive::new(Budget {
        inner: dec,
        left: 4 * MAX_BUNDLE,
    });
    let mut entries = 0usize;
    for entry in ar.entries()? {
        let mut e = entry?;
        entries += 1;
        if entries > 100_000 {
            return Err(std::io::Error::other("too many entries"));
        }
        if e.header().entry_type() != tar::EntryType::Regular {
            return Err(std::io::Error::other("only regular files are allowed"));
        }
        let rel = e.path()?.to_string_lossy().to_string();
        if !safe_relative(&rel) || rel.len() > 4096 {
            return Err(std::io::Error::other(format!("unsafe path {rel}")));
        }
        if e.size() > MAX_BUNDLE {
            return Err(std::io::Error::other("entry too large"));
        }
        let dest = out.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&dest)?;
        std::io::copy(&mut e, &mut f)?;
    }
    let m: Manifest = serde_json::from_slice(&std::fs::read(out.join("manifest.json"))?)?;
    Ok(m)
}

/// `(size, sha256 hex)` of a file.
pub fn hash_file(p: &Path) -> std::io::Result<(u64, String)> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut n = 0u64;
    loop {
        let r = f.read(&mut buf)?;
        if r == 0 {
            break;
        }
        n += r as u64;
        h.update(&buf[..r]);
    }
    Ok((n, hex(&h.finalize())))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `git@github.com:a/b.git` ≡ `https://github.com/a/b`.
/// `(host, path)` of a git remote: URL form `scheme://[user@]host[:port]/path` or scp form
/// `[user@]host:path`. Host is case-insensitive; the path is not.
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

pub fn same_remote(a: &str, b: &str) -> bool {
    matches!((remote_parts(a), remote_parts(b)), (Some(x), Some(y)) if x == y)
}

pub fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ if p == "~" => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
        _ => PathBuf::from(p),
    }
}

pub fn harness_home(harness: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    match harness {
        "claude" => Some(
            std::env::var_os("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude")),
        ),
        "codex" => Some(
            std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
        ),
        _ => None,
    }
}

pub fn claude_project_dir(cwd: &Path) -> String {
    cwd.display()
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub fn known_harness(h: &str) -> bool {
    matches!(h, "claude" | "codex" | "pi" | "omp")
}

pub fn valid_session_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Resume argv for a harness, built locally (spec 16 §15.2).
pub fn resume_args(harness: &str, session: Option<&str>) -> Option<Vec<String>> {
    let id = session.filter(|id| valid_session_id(id))?;
    match harness {
        "claude" => Some(vec!["--resume".into(), id.into()]),
        "codex" => Some(vec!["resume".into(), id.into()]),
        _ => None,
    }
}

/// Text from a manifest shown to people or agents: no control characters, bounded length.
pub fn clean(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_compare() {
        assert!(same_remote(
            "git@github.com:demo/vibeke.git",
            "https://github.com/MidgardAI/vibeke"
        ));
        assert!(same_remote(
            "https://GitHub.com/demo/vibeke/",
            "ssh://git@github.com/MidgardAI/vibeke.git"
        ));
        assert!(
            !same_remote(
                "https://github.com/the maintainer/vibeke",
                "https://github.com/MidgardAI/vibeke"
            ),
            "paths are case-sensitive"
        );
        assert!(!same_remote(
            "https://evil.example/path@github.com/org/repo",
            "git@github.com:org/repo.git"
        ));
        assert!(!same_remote(
            "git@github.com:demo/vibeke.git",
            "git@github.com:demo/other.git"
        ));
    }

    #[test]
    fn remote_matching() {
        assert!(same_remote("/srv/git/repo.git", "file:///srv/git/repo"));
        assert!(!same_remote("/srv/git/repo", "/srv/git/other"));
        assert!(!same_remote(
            "https://github.com/the maintainer/vibeke",
            "https://github.com/MidgardAI/vibeke"
        ));
        assert!(same_remote(
            "https://GitHub.com/demo/vibeke/",
            "git@github.com:demo/vibeke.git"
        ));
        assert!(!same_remote(
            "https://evil.example/path@github.com/org/repo",
            "git@github.com:org/repo.git"
        ));
    }

    #[test]
    fn claude_dirs() {
        assert_eq!(
            claude_project_dir(Path::new("/Users/demo/code/vibeke")),
            "-Users-demo-code-vibeke"
        );
        assert_eq!(claude_project_dir(Path::new("/a/b.c")), "-a-b-c");
        assert_eq!(
            transcript_relative("claude", "/h/.claude/projects/-a/x.jsonl").as_deref(),
            Some("projects/-a/x.jsonl")
        );
        assert_eq!(
            transcript_relative("codex", "/h/.codex/sessions/2026/10/06/r.jsonl").as_deref(),
            Some("sessions/2026/10/06/r.jsonl")
        );
    }

    #[test]
    fn resume_args_are_rebuilt_not_trusted() {
        assert_eq!(
            resume_args("claude", Some("abc-123")),
            Some(vec!["--resume".into(), "abc-123".into()])
        );
        assert_eq!(
            resume_args("codex", Some("u1")),
            Some(vec!["resume".into(), "u1".into()])
        );
        assert_eq!(
            resume_args("claude", Some("x --dangerously-skip-permissions")),
            None
        );
        assert_eq!(resume_args("claude", Some("../../etc")), None);
        assert_eq!(resume_args("evil", Some("x")), None);
    }

    #[test]
    fn untracked_writes_never_follow_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("wt");
        let outside = t.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let src = t.path().join("src");
        std::fs::write(&src, "x").unwrap();
        assert!(write_new_file(&root, "link/pwned", &src).is_err());
        assert!(!outside.join("pwned").exists());
        assert!(write_new_file(&root, "../escape", &src).is_err());
        write_new_file(&root, "a/b/c.txt", &src).unwrap();
        assert!(
            write_new_file(&root, "a/b/c.txt", &src).is_err(),
            "never replaces"
        );
    }

    #[test]
    fn pack_unpack_round_trip_with_sidechain() {
        let t = tempfile::tempdir().unwrap();
        let (work, root, out) = (t.path().join("w"), t.path().join("r"), t.path().join("o"));
        for d in [&work, &root, &out] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(work.join("changes.patch"), "").unwrap();
        std::fs::write(work.join("transcript.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(work.join("sidechain/subagents")).unwrap();
        std::fs::write(work.join("sidechain/subagents/a.jsonl"), "{\"a\":1}\n").unwrap();
        std::fs::write(root.join("notes.md"), "n\n").unwrap();
        let m = Manifest {
            v: 1,
            head: "a".repeat(40),
            bundle: "none".into(),
            untracked: vec!["notes.md".into()],
            ..Default::default()
        };
        let b = t.path().join("b.tar.zst");
        pack(&b, &work, &root, &m, false).unwrap();
        let got = unpack(&b, &out).unwrap();
        assert_eq!(got.head, m.head);
        assert_eq!(
            std::fs::read_to_string(out.join("sidechain/subagents/a.jsonl")).unwrap(),
            "{\"a\":1}\n"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("untracked/notes.md")).unwrap(),
            "n\n"
        );
        let (size, sha) = hash_file(&b).unwrap();
        assert_eq!(size, std::fs::metadata(&b).unwrap().len());
        assert_eq!(sha.len(), 64);
    }
}
