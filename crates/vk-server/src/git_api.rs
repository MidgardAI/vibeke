//! Read-only git methods for a pane's working tree (16 §7.7): `git.status` and `git.diff`.
//! The hardened runner and the no-follow file helpers are shared with `fs_api` (`git.log`,
//! `fs.list`, `fs.read`).
//!
//! Git can run configured programs (fsmonitor, external diff, textconv), so every call disables
//! them, runs with a timeout and an output cap, and untracked files are read without following
//! symlinks. Secret-looking files are reported without content (09 §9.1).

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use vk_proto::rpc::{ErrorKind, RpcError};

use crate::Server;
use crate::api::{Ctx, R, err, invalid, req, resolve_pane, s};

pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const MAX_DIFF: usize = 512 * 1024;
const MAX_UNTRACKED: u64 = 1024 * 1024;
const MAX_FILES: usize = 2000;
/// Untracked files whose lines are counted for `git.status` (and the total bytes read for it).
const MAX_UNTRACKED_COUNTED: usize = 200;
const UNTRACKED_COUNT_BUDGET: u64 = 8 * 1024 * 1024;

const SAFE_CONFIG: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.untrackedCache=false",
    "-c",
    "diff.external=",
    "-c",
    "core.pager=cat",
    "-c",
    "color.ui=false",
    "-c",
    "core.quotePath=false",
    // Never descend into submodules: their own config (filters) isn't neutralized.
    "-c",
    "submodule.recurse=false",
    "-c",
    "diff.ignoreSubmodules=all",
    "-c",
    "status.submoduleSummary=false",
    "-c",
    "diff.noprefix=false",
    // Signature checks run the configured gpg program.
    "-c",
    "log.showSignature=false",
];

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    match method {
        "git.status" => Some(status(server, ctx, p).await),
        "git.diff" => Some(diff(server, ctx, p).await),
        _ => None,
    }
}

pub(crate) fn not_a_repo() -> RpcError {
    err(ErrorKind::NotFound, "not_a_repo")
}

/// The directory to inspect: a pane's cwd, or an explicit `path` for full-scope callers.
pub(crate) fn target_dir(server: &Server, ctx: &Ctx, p: &Value) -> Result<PathBuf, RpcError> {
    if let Some(path) = s(p, "path") {
        if ctx.pane_scope.is_some() {
            return Err(err(
                ErrorKind::PermissionDenied,
                "path targets need full scope",
            ));
        }
        return Ok(PathBuf::from(path));
    }
    let pane = resolve_pane(server, ctx, s(p, "pane"))?;
    if ctx.pane_scope.as_deref().is_some_and(|own| own != pane.id) {
        return Err(err(
            ErrorKind::PermissionDenied,
            "git methods are limited to your own pane",
        ));
    }
    server
        .pane_cwd(&pane.id)
        .map(PathBuf::from)
        .ok_or_else(|| err(ErrorKind::NotFound, "pane has no known working directory"))
}

/// `-c filter.<name>.{clean,smudge,process}=` for every filter driver configured for this repo:
/// git runs clean/process filters during status and diff, and an empty command disables them.
/// Reading config runs nothing.
async fn filter_overrides(dir: &Path) -> Vec<String> {
    let out = tokio::time::timeout(
        TIMEOUT,
        tokio::process::Command::new("git")
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
            .output(),
    )
    .await;
    let Ok(Ok(out)) = out else { return Vec::new() };
    let mut names: Vec<String> = out
        .stdout
        .split(|&b| b == 0)
        .filter_map(|k| {
            let k = String::from_utf8_lossy(k);
            let rest = k.strip_prefix("filter.")?;
            rest.rsplit_once('.').map(|(name, _)| name.to_string())
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

pub(crate) async fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, RpcError> {
    match git_raw(dir, args, None, true).await? {
        (Some(0), out) => Ok(out),
        (_, out) if out.len() >= MAX_OUTPUT => Ok(out),
        _ => Err(not_a_repo()),
    }
}

/// The hardened runner: returns the exit code (None when killed by a signal) and capped stdout.
/// `input` is written to stdin (otherwise stdin is closed). `literal_pathspecs` is only off for
/// commands that reject it (`check-ignore`).
pub(crate) async fn git_raw(
    dir: &Path,
    args: &[&str],
    input: Option<Vec<u8>>,
    literal_pathspecs: bool,
) -> Result<(Option<i32>, Vec<u8>), RpcError> {
    let filters = filter_overrides(dir).await;
    let mut child = tokio::process::Command::new("git")
        .args(literal_pathspecs.then_some("--literal-pathspecs"))
        .args(SAFE_CONFIG)
        .args(&filters)
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| err(ErrorKind::Unsupported, format!("git: {e}")))?;
    if let (Some(data), Some(mut stdin)) = (input, child.stdin.take()) {
        // Written concurrently so a full stdout pipe can't deadlock the writer.
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(&data).await;
        });
    }
    let mut out = Vec::new();
    let mut stdout = child.stdout.take().expect("piped");
    let read = async {
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = stdout.read(&mut buf).await?;
            if n == 0 || out.len() >= MAX_OUTPUT {
                break;
            }
            out.extend_from_slice(&buf[..n.min(MAX_OUTPUT - out.len())]);
        }
        child.wait().await
    };
    match tokio::time::timeout(TIMEOUT, read).await {
        Ok(Ok(st)) => Ok((st.code(), out)),
        Ok(Err(e)) => Err(err(ErrorKind::Internal, format!("git: {e}"))),
        Err(_) => Err(err(ErrorKind::Timeout, "git timed out")),
    }
}

pub(crate) async fn repo_root(dir: &Path) -> Result<PathBuf, RpcError> {
    let out = git(dir, &["rev-parse", "--show-toplevel"]).await?;
    let root = String::from_utf8_lossy(&out).trim().to_string();
    if root.is_empty() {
        return Err(not_a_repo());
    }
    Ok(PathBuf::from(root))
}

#[derive(Debug, Clone, PartialEq)]
struct FileStatus {
    path: String,
    orig: Option<String>,
    x: char,
    y: char,
    kind: &'static str,
}

fn kind_of(x: char, y: char) -> &'static str {
    match (x, y) {
        ('?', _) => "untracked",
        ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D') => "conflicted",
        ('R', _) | (_, 'R') => "renamed",
        ('A', _) => "added",
        ('D', _) | (_, 'D') => "deleted",
        _ => "modified",
    }
}

#[derive(Default, Debug)]
struct Status {
    branch: Option<String>,
    upstream: Option<String>,
    ahead: i64,
    behind: i64,
    files: Vec<FileStatus>,
}

/// Parse `git status --porcelain=v2 -b -z`.
fn parse_status(out: &[u8]) -> Status {
    let mut st = Status::default();
    let mut it = out
        .split(|&b| b == 0)
        .map(|e| String::from_utf8_lossy(e).to_string());
    while let Some(e) = it.next() {
        if let Some(h) = e.strip_prefix("# branch.head ") {
            st.branch = (h != "(detached)").then(|| h.to_string());
        } else if let Some(u) = e.strip_prefix("# branch.upstream ") {
            st.upstream = Some(u.to_string());
        } else if let Some(ab) = e.strip_prefix("# branch.ab ") {
            let mut parts = ab.split(' ');
            st.ahead = parts
                .next()
                .and_then(|a| a.trim_start_matches('+').parse().ok())
                .unwrap_or(0);
            st.behind = parts
                .next()
                .and_then(|b| b.trim_start_matches('-').parse().ok())
                .unwrap_or(0);
        } else if let Some(rest) = e.strip_prefix("1 ") {
            let f: Vec<&str> = rest.splitn(8, ' ').collect();
            if let (Some(xy), Some(path)) = (f.first(), f.get(7)) {
                push(&mut st, xy, path, None);
            }
        } else if let Some(rest) = e.strip_prefix("2 ") {
            let f: Vec<&str> = rest.splitn(9, ' ').collect();
            let orig = it.next();
            if let (Some(xy), Some(path)) = (f.first(), f.get(8)) {
                push(&mut st, xy, path, orig);
            }
        } else if let Some(rest) = e.strip_prefix("u ") {
            let f: Vec<&str> = rest.splitn(10, ' ').collect();
            if let (Some(xy), Some(path)) = (f.first(), f.get(9)) {
                push(&mut st, xy, path, None);
            }
        } else if let Some(path) = e.strip_prefix("? ") {
            st.files.push(FileStatus {
                path: path.into(),
                orig: None,
                x: '?',
                y: '?',
                kind: "untracked",
            });
        }
    }
    st
}

fn push(st: &mut Status, xy: &str, path: &str, orig: Option<String>) {
    let mut c = xy.chars();
    let (x, y) = (c.next().unwrap_or('.'), c.next().unwrap_or('.'));
    st.files.push(FileStatus {
        path: path.into(),
        orig,
        x,
        y,
        kind: kind_of(x, y),
    });
}

/// Parse `git diff --numstat -z`: path → (adds, dels) or binary.
pub(crate) fn parse_numstat(out: &[u8]) -> Vec<(String, Option<(u64, u64)>)> {
    let mut res = Vec::new();
    let mut it = out
        .split(|&b| b == 0)
        .map(|e| String::from_utf8_lossy(e).to_string());
    while let Some(e) = it.next() {
        let mut f = e.splitn(3, '\t');
        let (Some(a), Some(d), Some(path)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        let path = if path.is_empty() {
            let _old = it.next();
            it.next().unwrap_or_default()
        } else {
            path.to_string()
        };
        let counts = a.parse().ok().zip(d.parse().ok());
        res.push((path, counts));
    }
    res
}

pub fn is_secret_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    name == ".env"
        || name.starts_with(".env.")
        || [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks"]
            .iter()
            .any(|ext| name.ends_with(ext))
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

/// A relative path with only normal components.
pub(crate) fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

async fn load_status(dir: &Path) -> Result<(PathBuf, Status), RpcError> {
    let root = repo_root(dir).await?;
    let out = git(
        &root,
        &[
            "status",
            "--porcelain=v2",
            "-b",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=all",
        ],
    )
    .await?;
    Ok((root, parse_status(&out)))
}

async fn status(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let dir = target_dir(server, ctx, p)?;
    let (root, st) = load_status(&dir).await?;
    let numstat = git(
        &root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--numstat",
            "-z",
            "HEAD",
        ],
    )
    .await
    .map(|o| parse_numstat(&o))
    .unwrap_or_default();
    let truncated = st.files.len() > MAX_FILES;
    // Untracked text files have no numstat: count their lines (no-follow reads, bounded).
    let untracked: Vec<String> = st
        .files
        .iter()
        .take(MAX_FILES)
        .filter(|f| f.kind == "untracked" && !is_secret_path(&f.path) && safe_relative(&f.path))
        .take(MAX_UNTRACKED_COUNTED)
        .map(|f| f.path.clone())
        .collect();
    let root2 = root.clone();
    let counted: Vec<(String, Option<(u64, u64)>)> = tokio::task::spawn_blocking(move || {
        let mut budget = UNTRACKED_COUNT_BUDGET;
        untracked
            .into_iter()
            .filter_map(|path| {
                let bytes = read_untracked(&root2, &path).ok().flatten()?;
                if budget < bytes.len() as u64 {
                    return None;
                }
                budget -= bytes.len() as u64;
                if bytes.contains(&0) {
                    return Some((path, None)); // binary
                }
                let lines = bytes.iter().filter(|&&b| b == b'\n').count() as u64
                    + u64::from(!bytes.is_empty() && !bytes.ends_with(b"\n"));
                Some((path, Some((lines, 0))))
            })
            .collect()
    })
    .await
    .unwrap_or_default();
    let files: Vec<Value> = st
        .files
        .iter()
        .take(MAX_FILES)
        .map(|f| {
            let counts = numstat
                .iter()
                .chain(counted.iter())
                .find(|(p, _)| *p == f.path)
                .map(|(_, c)| *c);
            let binary = matches!(counts, Some(None));
            let (adds, dels) = counts.flatten().map_or((None, None), |(a, d)| (Some(a), Some(d)));
            json!({"path": f.path, "orig_path": f.orig, "x": f.x.to_string(), "y": f.y.to_string(), "kind": f.kind,
                   "staged": f.x != '.' && f.x != '?', "adds": adds, "dels": dels, "binary": binary, "secret": is_secret_path(&f.path)})
        })
        .collect();
    Ok(
        json!({"repo_root": root, "branch": st.branch, "upstream": st.upstream, "ahead": st.ahead, "behind": st.behind,
              "clean": files.is_empty(), "files": files, "truncated": truncated}),
    )
}

fn denied() -> RpcError {
    err(
        ErrorKind::PermissionDenied,
        "not a regular file inside the repository",
    )
}

fn open_at(
    dir: libc::c_int,
    name: &[u8],
    flags: libc::c_int,
) -> Result<std::os::fd::OwnedFd, RpcError> {
    use std::os::fd::FromRawFd;
    let c = std::ffi::CString::new(name).map_err(|_| denied())?;
    // SAFETY: `c` is a valid NUL-terminated string; the returned fd is owned below.
    let fd = unsafe { libc::openat(dir, c.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(denied());
    }
    // SAFETY: openat returned a fresh descriptor we own.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

/// Open the directory `rel` under `root` ("" = `root` itself), walking with directory
/// descriptors and `O_NOFOLLOW` at every component, so a directory swapped for a symlink
/// mid-walk cannot redirect access outside the repository.
pub(crate) fn open_dir_nofollow(root: &Path, rel: &str) -> Result<std::os::fd::OwnedFd, RpcError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let mut dir = open_at(
        libc::AT_FDCWD,
        root.as_os_str().as_bytes(),
        libc::O_RDONLY | libc::O_DIRECTORY,
    )?;
    for d in rel.split('/').filter(|d| !d.is_empty()) {
        dir = open_at(
            dir.as_raw_fd(),
            d.as_bytes(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )?;
    }
    Ok(dir)
}

/// Open `rel` under `root` without following symlinks at any component (the leaf may be any
/// non-symlink type; callers check it with `fstat`).
pub(crate) fn open_nofollow(root: &Path, rel: &str) -> Result<std::fs::File, RpcError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let (dirs, leaf) = rel.rsplit_once('/').unwrap_or(("", rel));
    if leaf.is_empty() {
        return Err(invalid("empty path"));
    }
    let dir = open_dir_nofollow(root, dirs)?;
    let fd = open_at(
        dir.as_raw_fd(),
        std::ffi::OsStr::new(leaf).as_bytes(),
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
    )?;
    Ok(std::fs::File::from(fd))
}

/// Read an untracked file without following symlinks at any component.
fn read_untracked(root: &Path, rel: &str) -> Result<Option<Vec<u8>>, RpcError> {
    let f = open_nofollow(root, rel)?;
    let md = f.metadata().map_err(|_| denied())?;
    if !md.is_file() || md.len() > MAX_UNTRACKED {
        return Ok(None);
    }
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(&f, MAX_UNTRACKED + 1), &mut buf)
        .map_err(|e| err(ErrorKind::Internal, e.to_string()))?;
    Ok(Some(buf))
}

async fn diff(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    if p.get("base").is_some() || p.get("range").is_some() {
        return crate::fs_api::diff_revs(server, ctx, p).await;
    }
    let file = req(p, "file")?;
    if !safe_relative(file) {
        return Err(invalid(
            "file must be a relative path inside the repository",
        ));
    }
    let dir = target_dir(server, ctx, p)?;
    let (root, st) = load_status(&dir).await?;
    let Some(entry) = st.files.iter().find(|f| f.path == file) else {
        return Err(err(ErrorKind::NotFound, "file has no changes"));
    };
    if is_secret_path(file) || entry.orig.as_deref().is_some_and(is_secret_path) {
        return Ok(
            json!({"file": file, "secret": true, "diff": "", "truncated": false, "binary": false, "untracked": entry.kind == "untracked"}),
        );
    }
    if entry.kind == "untracked" {
        let root2 = root.clone();
        let rel = file.to_string();
        let bytes = tokio::task::spawn_blocking(move || read_untracked(&root2, &rel))
            .await
            .map_err(|e| err(ErrorKind::Internal, e.to_string()))??;
        let Some(bytes) = bytes else {
            return Ok(
                json!({"file": file, "diff": "", "truncated": true, "binary": false, "untracked": true}),
            );
        };
        if bytes.contains(&0) {
            return Ok(
                json!({"file": file, "diff": "", "truncated": false, "binary": true, "untracked": true}),
            );
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let mut d = format!(
            "--- /dev/null\n+++ b/{file}\n@@ -0,0 +1,{} @@\n",
            lines.len()
        );
        for l in &lines {
            d.push('+');
            d.push_str(l);
            d.push('\n');
        }
        let truncated = d.len() > MAX_DIFF;
        d.truncate(floor_char(&d, MAX_DIFF));
        return Ok(
            json!({"file": file, "diff": d, "truncated": truncated, "binary": false, "untracked": true}),
        );
    }
    let staged = p.get("staged").and_then(Value::as_bool).unwrap_or(false);
    let mut args = vec!["diff", "--no-ext-diff", "--no-textconv", "--no-color"];
    if staged {
        args.push("--cached");
    } else {
        args.push("HEAD");
    }
    args.push("--");
    args.push(file);
    if let Some(orig) = &entry.orig {
        args.push(orig);
    }
    let out = git(&root, &args).await?;
    let text = String::from_utf8_lossy(&out).to_string();
    let binary = text.lines().any(|l| l.starts_with("Binary files "));
    let truncated = text.len() > MAX_DIFF;
    let mut d = text;
    d.truncate(floor_char(&d, MAX_DIFF));
    Ok(
        json!({"file": file, "diff": d, "truncated": truncated, "binary": binary, "untracked": false}),
    )
}

pub(crate) fn floor_char(s: &str, max: usize) -> usize {
    if s.len() <= max {
        return s.len();
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2() {
        let out = b"# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaa bbb src/lib.rs\0\
2 R. N... 100644 100644 100644 aaa bbb R100 new name.rs\0old.rs\0\
u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
? notes.txt\0";
        let st = parse_status(out);
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        let kinds: Vec<_> = st.files.iter().map(|f| (f.path.as_str(), f.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                ("src/lib.rs", "modified"),
                ("new name.rs", "renamed"),
                ("conflict.rs", "conflicted"),
                ("notes.txt", "untracked")
            ]
        );
        assert_eq!(st.files[1].orig.as_deref(), Some("old.rs"));
    }

    #[test]
    fn parses_numstat() {
        let n = parse_numstat(b"3\t1\tsrc/a.rs\0-\t-\timg.png\0" as &[u8]);
        assert_eq!(n[0], ("src/a.rs".into(), Some((3, 1))));
        assert_eq!(n[1], ("img.png".into(), None));
        let r = parse_numstat(b"1\t0\t\0old.rs\0new.rs\0");
        assert_eq!(r[0].0, "new.rs");
    }

    #[test]
    fn secrets_and_paths() {
        assert!(is_secret_path(".env"));
        assert!(is_secret_path("app/.env.local"));
        assert!(is_secret_path("certs/server.pem"));
        assert!(is_secret_path("home/.ssh/id_ed25519.pub"));
        assert!(!is_secret_path("src/environment.rs"));
        assert!(safe_relative("src/a.rs"));
        assert!(!safe_relative("../etc/passwd"));
        assert!(!safe_relative("/etc/passwd"));
        assert!(!safe_relative("a/./b"));
    }

    fn run(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[tokio::test]
    async fn real_repo_status_diff_and_hostile_config() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        run(d, &["init", "-q", "-b", "main"]);
        run(d, &["config", "user.email", "t@example.com"]);
        run(d, &["config", "user.name", "t"]);
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        std::fs::write(d.join("*"), "star\n").unwrap();
        std::fs::write(d.join(".env"), "TOKEN=old\n").unwrap();
        run(d, &["add", "a.txt", ":(literal)*", ".env"]);
        run(d, &["commit", "-qm", "init"]);
        std::fs::write(d.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(d.join("*"), "star\nchanged\n").unwrap();
        std::fs::create_dir(d.join("real")).unwrap();
        std::fs::write(d.join("real/x.txt"), "inside\n").unwrap();
        std::os::unix::fs::symlink(d.join("real"), d.join("sub")).unwrap();
        std::fs::write(d.join("new.txt"), "hello\n").unwrap();
        std::fs::write(d.join(".env"), "TOKEN=secret\n").unwrap();
        std::os::unix::fs::symlink("/etc/hosts", d.join("link.txt")).unwrap();
        // Hostile repo config: these must never run.
        let marker = d.join("pwned");
        let script = d.join("evil.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        run(d, &["config", "core.fsmonitor", script.to_str().unwrap()]);
        run(d, &["config", "diff.external", script.to_str().unwrap()]);
        std::fs::write(d.join(".gitattributes"), "*.txt diff=evil filter=evil\n").unwrap();
        run(
            d,
            &["config", "diff.evil.textconv", script.to_str().unwrap()],
        );
        run(
            d,
            &["config", "filter.evil.clean", script.to_str().unwrap()],
        );
        run(
            d,
            &["config", "filter.evil.process", script.to_str().unwrap()],
        );

        let (root, st) = load_status(d).await.unwrap();
        assert_eq!(root.canonicalize().unwrap(), d.canonicalize().unwrap());
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert!(
            st.files
                .iter()
                .any(|f| f.path == "a.txt" && f.kind == "modified")
        );
        assert!(
            st.files
                .iter()
                .any(|f| f.path == "new.txt" && f.kind == "untracked")
        );

        let out = git(
            &root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "HEAD",
                "--",
                "a.txt",
            ],
        )
        .await
        .unwrap();
        assert!(String::from_utf8_lossy(&out).contains("+two"));
        assert_eq!(
            read_untracked(&root, "new.txt").unwrap().unwrap(),
            b"hello\n"
        );
        assert!(read_untracked(&root, "link.txt").is_err());
        assert!(
            read_untracked(&root, "sub/x.txt").is_err(),
            "symlinked directory followed"
        );
        assert_eq!(
            read_untracked(&root, "real/x.txt").unwrap().unwrap(),
            b"inside\n"
        );
        // A file named `*` must not act as a pathspec that pulls in `.env`.
        let star = git(
            &root,
            &["diff", "--no-ext-diff", "--no-textconv", "HEAD", "--", "*"],
        )
        .await
        .unwrap();
        let star = String::from_utf8_lossy(&star);
        assert!(star.contains("+changed"));
        assert!(
            !star.contains("TOKEN"),
            "pathspec magic leaked .env: {star}"
        );
        assert!(!marker.exists(), "repo-configured program ran");
    }
}
