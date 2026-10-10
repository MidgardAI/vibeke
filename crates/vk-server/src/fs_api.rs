//! Read-only repository browsing for a pane's working tree: `git.log`, `git.diff` against a
//! base or over a range, `fs.list` and `fs.read` (07 §2.15a, 16 §7.7).
//!
//! Same rules as `git.status` / `git.diff`: git runs through the hardened runner (filters,
//! external diff, textconv, fsmonitor and signature programs disabled, literal pathspecs, no
//! submodule recursion, timeout and output cap); paths are relative to the repository root and
//! resolved with an `openat` walk that never follows a symlink; secret-looking files are listed
//! but never read (09 §9.1); `.git` is neither listed nor readable; pane-scope callers may only
//! target their own pane.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use vk_proto::rpc::{ErrorKind, RpcError};

use crate::Server;
use crate::api::{Ctx, R, err, invalid, s, u};
use crate::git_api::{
    floor_char, git, git_raw, is_secret_path, open_dir_nofollow, open_nofollow, repo_root,
    safe_relative, target_dir,
};

const LOG_DEFAULT: u64 = 50;
const LOG_MAX: u64 = 200;
const MAX_ENTRIES: usize = 2000;
/// Entries read from one directory before sorting; beyond this the listing is truncated.
const MAX_SCAN: usize = 100_000;
const MAX_READ: u64 = 512 * 1024;
/// Largest image `fs.read` with `as: "image"` returns inline (base64 grows it by a third; the
/// gateway channel carries messages up to 16 MiB).
const MAX_IMAGE: u64 = 4 * 1024 * 1024;
const MAX_DIFF: usize = 512 * 1024;
const SNIFF: usize = 8 * 1024;

/// Registered in `api::METHODS` (all read-only).
pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if !matches!(method, "git.log" | "fs.list" | "fs.read") {
        return None;
    }
    // For fs.*, `path` is the entry inside the repository, never `target_dir`'s directory
    // override, so the repository always comes from the pane.
    let mut target = p.clone();
    if method.starts_with("fs.")
        && let Some(o) = target.as_object_mut()
    {
        o.remove("path");
    }
    let root = match root_for(server, ctx, &target).await {
        Ok(root) => root,
        Err(e) => return Some(Err(e)),
    };
    let p = p.clone();
    Some(match method {
        "git.log" => log(root, p).await,
        "fs.list" => list(root, p).await,
        _ => read(root, p).await,
    })
}

// ---- refs ----------------------------------------------------------------------------------

/// A single revision: `^[A-Za-z0-9][A-Za-z0-9._/@{}^~-]{0,200}$` without `..` (so never an
/// option, never a range).
pub(crate) fn valid_ref(r: &str) -> bool {
    let mut chars = r.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && r.len() <= 201
        && chars.all(|c| c.is_ascii_alphanumeric() || "._/@{}^~-".contains(c))
        && !r.contains("..")
}

/// `a..b` or `a...b` with both sides valid refs → the range string to pass to git.
pub(crate) fn valid_range(r: &str) -> Option<String> {
    let (a, sep, b) = if let Some((a, b)) = r.split_once("...") {
        (a, "...", b)
    } else {
        let (a, b) = r.split_once("..")?;
        (a, "..", b)
    };
    (valid_ref(a) && valid_ref(b)).then(|| format!("{a}{sep}{b}"))
}

fn bad_ref(what: &str) -> RpcError {
    invalid(format!("{what} is not a valid git revision")).details(json!({"reason": "invalid_ref"}))
}

fn param_ref<'a>(p: &'a Value, k: &str) -> Result<Option<&'a str>, RpcError> {
    match p.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(r)) if valid_ref(r) => Ok(Some(r)),
        Some(_) => Err(bad_ref(k)),
    }
}

async fn root_for(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> Result<PathBuf, RpcError> {
    let dir = target_dir(server, ctx, p)?;
    repo_root(&dir).await
}

// ---- git.log -------------------------------------------------------------------------------

fn parse_log(out: &[u8]) -> Vec<Value> {
    out.split(|&b| b == 0)
        .filter_map(|rec| {
            let rec = String::from_utf8_lossy(rec);
            let rec = rec.trim_start_matches('\n');
            let mut f = rec.splitn(5, '\x1f');
            let (sha, short, author, at, subject) =
                (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
            if sha.is_empty() {
                return None;
            }
            Some(json!({"sha": sha, "short": short, "author": author,
                        "ts": at.trim().parse::<i64>().ok().map(|s| s * 1000), "subject": subject}))
        })
        .collect()
}

async fn log(root: PathBuf, p: Value) -> R {
    let p = &p;
    let base = param_ref(p, "base")?;
    let limit = u(p, "limit").unwrap_or(LOG_DEFAULT).clamp(1, LOG_MAX);
    let n = format!("-n{}", limit + 1);
    let rev = base.map_or_else(|| "HEAD".to_string(), |b| format!("{b}..HEAD"));
    let out = git(
        &root,
        &[
            "log",
            "-z",
            "--no-show-signature",
            "--no-color",
            "--format=%H%x1f%h%x1f%an%x1f%at%x1f%s",
            &n,
            "--end-of-options",
            &rev,
            "--",
        ],
    )
    .await?;
    let mut commits = parse_log(&out);
    let truncated = commits.len() as u64 > limit;
    commits.truncate(limit as usize);
    Ok(json!({"commits": commits, "truncated": truncated}))
}

// ---- git.diff {base | range} ---------------------------------------------------------------

/// `git.diff` with `base` (that revision against the working tree) or `range` (`a..b`,
/// `a...b`). With `file`: that file's diff. Without: the changed files with counts.
pub(crate) async fn diff_revs(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let root = root_for(server, ctx, p).await?;
    Box::pin(diff_revs_at(&root, p)).await
}

async fn diff_revs_at(root: &Path, p: &Value) -> R {
    let rev = match (p.get("base"), p.get("range")) {
        (Some(_), Some(_)) => return Err(invalid("pass either `base` or `range`, not both")),
        (Some(_), None) => param_ref(p, "base")?.unwrap_or_default().to_string(),
        (None, Some(r)) => r
            .as_str()
            .and_then(valid_range)
            .ok_or_else(|| bad_ref("range"))?,
        (None, None) => return Err(invalid("`base` or `range` required")),
    };
    let file = s(p, "file");
    if let Some(f) = file
        && !safe_relative(f)
    {
        return Err(invalid(
            "file must be a relative path inside the repository",
        ));
    }
    let Some(file) = file else {
        let out = git(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--numstat",
                "-z",
                "-M",
                "--end-of-options",
                &rev,
                "--",
            ],
        )
        .await?;
        let all = crate::git_api::parse_numstat(&out);
        // Change kind per path (A/M/D/R/C/T) and rename sources, from --name-status.
        let names = git(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--name-status",
                "-z",
                "-M",
                "--end-of-options",
                &rev,
                "--",
            ],
        )
        .await
        .map(|o| parse_name_status(&o))
        .unwrap_or_default();
        let truncated = all.len() > MAX_ENTRIES;
        let files: Vec<Value> = all
            .into_iter()
            .take(MAX_ENTRIES)
            .map(|(path, c)| {
                let ns = names.iter().find(|n| n.path == path);
                json!({"path": path, "adds": c.map(|c| c.0), "dels": c.map(|c| c.1),
                       "binary": c.is_none(), "secret": is_secret_path(&path),
                       "status": ns.map(|n| n.status.to_string()), "orig_path": ns.and_then(|n| n.orig.clone())})
            })
            .collect();
        return Ok(json!({"rev": rev, "files": files, "truncated": truncated}));
    };
    // The file must be exactly one changed path in this comparison: a directory (or any other
    // pathspec) would otherwise pull in everything beneath it, secrets included.
    let changed = git(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--name-status",
            "-z",
            "-M",
            "--end-of-options",
            &rev,
            "--",
        ],
    )
    .await
    .map(|o| parse_name_status(&o))?;
    let Some(entry) = changed
        .iter()
        .find(|n| n.path == file || n.orig.as_deref() == Some(file))
    else {
        return Err(err(
            ErrorKind::NotFound,
            "file has no changes in this comparison",
        ));
    };
    if is_secret_path(&entry.path) || entry.orig.as_deref().is_some_and(is_secret_path) {
        return Ok(
            json!({"file": file, "rev": rev, "secret": true, "diff": "", "truncated": false, "binary": false, "untracked": false}),
        );
    }
    let out = git(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--end-of-options",
            &rev,
            "--",
            file,
        ],
    )
    .await?;
    let text = String::from_utf8_lossy(&out).to_string();
    let binary = text.lines().any(|l| l.starts_with("Binary files "));
    let truncated = text.len() > MAX_DIFF;
    let mut d = text;
    d.truncate(floor_char(&d, MAX_DIFF));
    Ok(
        json!({"file": file, "rev": rev, "diff": d, "truncated": truncated, "binary": binary, "untracked": false}),
    )
}

// ---- fs.list / fs.read ---------------------------------------------------------------------

/// Validate a path param ("" allowed only when `allow_root`): safe relative, no `.git`
/// component. Returns whether any component is secret-looking.
fn check_path(path: &str, allow_root: bool) -> Result<bool, RpcError> {
    if path.is_empty() {
        return if allow_root {
            Ok(false)
        } else {
            Err(invalid("path required"))
        };
    }
    if !safe_relative(path) {
        return Err(invalid(
            "path must be a relative path inside the repository",
        ));
    }
    // Case-insensitive: on case-insensitive volumes `.GIT` is the same directory.
    if path.split('/').any(|c| c.eq_ignore_ascii_case(".git")) {
        return Err(err(
            ErrorKind::PermissionDenied,
            "the .git directory is not browsable",
        ));
    }
    Ok(path.split('/').any(is_secret_path))
}

#[derive(Debug)]
struct Entry {
    name: String,
    kind: &'static str,
    size: Option<u64>,
}

/// One directory level under `root`, read through a no-follow descriptor walk; entry types come
/// from `fstatat(AT_SYMLINK_NOFOLLOW)`, so symlinks are reported, never followed.
fn read_dir_nofollow(root: &Path, rel: &str) -> Result<(Vec<Entry>, bool), RpcError> {
    use std::ffi::CStr;
    use std::os::fd::AsRawFd;
    let dir = open_dir_nofollow(root, rel)?;
    // `fdopendir` takes ownership of a duplicate; `dir` stays valid for `fstatat`.
    // SAFETY: dup of a valid descriptor.
    let dup = unsafe { libc::dup(dir.as_raw_fd()) };
    if dup < 0 {
        return Err(err(ErrorKind::Internal, "dup failed"));
    }
    // SAFETY: `dup` is a fresh directory descriptor; on success the DIR owns it.
    let d = unsafe { libc::fdopendir(dup) };
    if d.is_null() {
        // SAFETY: fdopendir failed, so we still own `dup`.
        unsafe { libc::close(dup) };
        return Err(err(ErrorKind::Internal, "fdopendir failed"));
    }
    let mut entries = Vec::new();
    let mut over = false;
    loop {
        // SAFETY: `d` is a valid DIR*; readdir returns null at the end.
        let ent = unsafe { libc::readdir(d) };
        if ent.is_null() {
            break;
        }
        // SAFETY: d_name is NUL-terminated within the dirent.
        let name_c = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
        let bytes = name_c.to_bytes();
        if bytes == b"." || bytes == b".." || bytes.eq_ignore_ascii_case(b".git") {
            continue;
        }
        if entries.len() >= MAX_SCAN {
            over = true;
            break;
        }
        let Ok(name) = std::str::from_utf8(bytes) else {
            continue; // not addressable through the JSON API
        };
        // SAFETY: zeroed stat is a valid out-parameter.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: valid dir fd, NUL-terminated name, valid out pointer.
        let rc = unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name_c.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            continue;
        }
        let (kind, size) = match st.st_mode & libc::S_IFMT {
            libc::S_IFDIR => ("dir", None),
            libc::S_IFLNK => ("symlink", None),
            libc::S_IFREG => ("file", Some(st.st_size as u64)),
            _ => ("other", None),
        };
        entries.push(Entry {
            name: name.to_string(),
            kind,
            size,
        });
    }
    // SAFETY: closes the DIR and its (duplicated) descriptor exactly once.
    unsafe { libc::closedir(d) };
    Ok((entries, over))
}

/// Which of `paths` (relative to `root`) git ignores, via `git check-ignore --stdin -z`.
async fn ignored(root: &Path, paths: &[String]) -> std::collections::HashSet<String> {
    if paths.is_empty() {
        return Default::default();
    }
    // check-ignore rejects `--literal-pathspecs`; a name starting with `:` would be read as
    // pathspec magic, so such names are not queried (reported as not ignored).
    let mut input = Vec::new();
    for p in paths.iter().filter(|p| !p.starts_with(':')) {
        input.extend_from_slice(p.as_bytes());
        input.push(0);
    }
    match git_raw(root, &["check-ignore", "--stdin", "-z"], Some(input), false).await {
        // 0: some ignored; 1: none ignored.
        Ok((Some(0), out)) => out
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).to_string())
            .collect(),
        _ => Default::default(),
    }
}

async fn list(root: PathBuf, p: Value) -> R {
    let path = s(&p, "path").unwrap_or("").trim_end_matches('/');
    let secret = check_path(path, true)?;
    if secret {
        return Ok(json!({"path": path, "secret": true, "entries": [], "truncated": false}));
    }
    let (root2, rel) = (root.clone(), path.to_string());
    let (mut entries, over) = tokio::task::spawn_blocking(move || read_dir_nofollow(&root2, &rel))
        .await
        .map_err(|e| err(ErrorKind::Internal, e.to_string()))??;
    entries.sort_by(|a, b| {
        (a.kind != "dir")
            .cmp(&(b.kind != "dir"))
            .then_with(|| a.name.cmp(&b.name))
    });
    let truncated = over || entries.len() > MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    let join = |n: &str| {
        if path.is_empty() {
            n.to_string()
        } else {
            format!("{path}/{n}")
        }
    };
    let rels: Vec<String> = entries.iter().map(|e| join(&e.name)).collect();
    let ign = ignored(&root, &rels).await;
    let out: Vec<Value> = entries
        .iter()
        .zip(&rels)
        .map(|(e, rel)| {
            let secret = is_secret_path(&e.name);
            let mut v = json!({"name": e.name, "kind": e.kind, "ignored": ign.contains(rel), "secret": secret});
            if let (Some(size), false) = (e.size, secret) {
                v["size"] = json!(size);
            }
            v
        })
        .collect();
    Ok(json!({"path": path, "entries": out, "truncated": truncated}))
}

/// MIME type for a png, jpeg, gif or webp file: the extension must name the type and the
/// leading bytes must agree, so a renamed file is never served as an image.
pub(crate) fn image_mime(name: &str, head: &[u8]) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    match ext.as_str() {
        "png" if head.starts_with(b"\x89PNG\r\n\x1a\n") => Some("image/png"),
        "jpg" | "jpeg" if head.starts_with(&[0xFF, 0xD8, 0xFF]) => Some("image/jpeg"),
        "gif" if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") => Some("image/gif"),
        "webp" if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" => {
            Some("image/webp")
        }
        _ => None,
    }
}

/// Read up to [`MAX_READ`] bytes of a regular file under `root` (no symlink anywhere).
fn read_file_capped(root: &Path, rel: &str, cap: u64) -> Result<(u64, Vec<u8>), RpcError> {
    let f = open_nofollow(root, rel)?;
    let md = f
        .metadata()
        .map_err(|e| err(ErrorKind::Internal, e.to_string()))?;
    if md.is_dir() {
        return Err(invalid("path is a directory (use fs.list)"));
    }
    if !md.is_file() {
        return Err(err(ErrorKind::PermissionDenied, "not a regular file"));
    }
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(&f, cap), &mut buf)
        .map_err(|e| err(ErrorKind::Internal, e.to_string()))?;
    Ok((md.len(), buf))
}

async fn read(root: PathBuf, p: Value) -> R {
    let path = s(&p, "path").unwrap_or("");
    let secret = check_path(path, false)?;
    if secret {
        return Ok(
            json!({"path": path, "secret": true, "binary": false, "truncated": false, "size": Value::Null}),
        );
    }
    let want_image = s(&p, "as") == Some("image");
    let (root2, rel) = (root.clone(), path.to_string());
    let (size, bytes) = tokio::task::spawn_blocking(move || {
        read_file_capped(&root2, &rel, if want_image { MAX_IMAGE } else { MAX_READ })
    })
    .await
    .map_err(|e| err(ErrorKind::Internal, e.to_string()))??;
    if want_image && let Some(mime) = image_mime(path, &bytes) {
        if size > bytes.len() as u64 {
            // Too big to send inline: the caller sees a binary file without data.
            return Ok(
                json!({"path": path, "binary": true, "truncated": true, "size": size, "secret": false, "mime": mime}),
            );
        }
        use base64::Engine;
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        return Ok(
            json!({"path": path, "binary": true, "truncated": false, "size": size, "secret": false, "mime": mime, "data_b64": data_b64}),
        );
    }
    let bytes = if bytes.len() as u64 > MAX_READ {
        bytes[..MAX_READ as usize].to_vec()
    } else {
        bytes
    };
    let truncated = size > bytes.len() as u64;
    if bytes[..bytes.len().min(SNIFF)].contains(&0) {
        return Ok(
            json!({"path": path, "binary": true, "truncated": truncated, "size": size, "secret": false}),
        );
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        // A cut multi-byte sequence at the end decodes as U+FFFD; drop it.
        while text.ends_with('\u{FFFD}') {
            text.pop();
        }
    }
    Ok(
        json!({"path": path, "text": text, "binary": false, "truncated": truncated, "size": size, "secret": false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repo with three commits, a gitignore, a secret, symlinks and an outside directory.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("repo");
        std::fs::create_dir(&d).unwrap();
        run(&d, &["init", "-q", "-b", "main"]);
        run(&d, &["config", "user.email", "t@example.com"]);
        run(&d, &["config", "user.name", "Tess"]);
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        std::fs::write(d.join(".gitignore"), "target/\n*.log\n").unwrap();
        run(&d, &["add", "."]);
        run(&d, &["commit", "-qm", "first"]);
        std::fs::write(d.join("a.txt"), "one\ntwo\n").unwrap();
        run(&d, &["commit", "-qam", "second"]);
        std::fs::create_dir(d.join("src")).unwrap();
        std::fs::write(d.join("src/lib.rs"), "fn x() {}\n").unwrap();
        run(&d, &["add", "src"]);
        run(&d, &["commit", "-qm", "third: add src"]);
        std::fs::write(d.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::create_dir(d.join("target")).unwrap();
        std::fs::write(d.join("target/out.bin"), "x").unwrap();
        std::fs::write(d.join("build.log"), "log").unwrap();
        std::fs::write(d.join(".env"), "TOKEN=secret\n").unwrap();
        std::fs::create_dir(d.join("certs")).unwrap();
        std::fs::write(d.join("certs/server.pem"), "PRIVATE").unwrap();
        std::fs::write(d.join("bin.dat"), b"ab\0cd").unwrap();
        let outside = t.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("x.txt"), "outside\n").unwrap();
        std::os::unix::fs::symlink(&outside, d.join("linkdir")).unwrap();
        std::os::unix::fs::symlink(outside.join("x.txt"), d.join("link.txt")).unwrap();
        let root = d.canonicalize().unwrap();
        (t, root)
    }

    fn kind(e: &RpcError) -> String {
        e.data.kind.clone()
    }

    #[test]
    fn refs_and_ranges() {
        for ok in [
            "main",
            "HEAD~2",
            "origin/main",
            "v1.2.3",
            "HEAD^",
            "main@{1}",
            "a1b2c3",
        ] {
            assert!(valid_ref(ok), "{ok}");
        }
        for bad in [
            "",
            "-p",
            "--output=x",
            "a..b",
            "a b",
            "x;rm",
            ".hidden",
            "a\0b",
        ] {
            assert!(!valid_ref(bad), "{bad}");
        }
        assert!(!valid_ref(&"a".repeat(202)));
        assert_eq!(valid_range("main..HEAD").as_deref(), Some("main..HEAD"));
        assert_eq!(valid_range("a...b").as_deref(), Some("a...b"));
        for bad in [
            "a..b..c",
            "..b",
            "a..",
            "-p..b",
            "a..--output=x",
            "main",
            "a....b",
        ] {
            assert!(valid_range(bad).is_none(), "{bad}");
        }
    }

    #[tokio::test]
    async fn log_with_base_limit_and_bad_refs() {
        let (_t, root) = fixture();
        let v = log(root.clone(), json!({})).await.unwrap();
        let commits = v["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 3);
        assert_eq!(v["truncated"], false);
        assert_eq!(commits[0]["subject"], "third: add src");
        assert_eq!(commits[0]["author"], "Tess");
        assert_eq!(commits[0]["sha"].as_str().unwrap().len(), 40);
        assert!(commits[0]["ts"].as_i64().unwrap() > 1_000_000_000_000);
        let v = log(root.clone(), json!({"limit": 2})).await.unwrap();
        assert_eq!(v["commits"].as_array().unwrap().len(), 2);
        assert_eq!(v["truncated"], true);
        let v = log(root.clone(), json!({"base": "HEAD~2"})).await.unwrap();
        let subjects: Vec<_> = v["commits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["subject"].as_str().unwrap())
            .collect();
        assert_eq!(subjects, ["third: add src", "second"]);
        for bad in ["--output=x", "-p", "a..b", "a..b..c"] {
            let e = log(root.clone(), json!({"base": bad})).await.unwrap_err();
            assert_eq!(kind(&e), "invalid_params", "{bad}");
        }
        assert!(!root.join("x").exists(), "--output must never reach git");
    }

    #[tokio::test]
    async fn repo_configured_programs_never_run() {
        let (t, root) = fixture();
        let marker = t.path().join("pwned");
        let script = t.path().join("evil.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let sc = script.to_str().unwrap();
        run(&root, &["config", "core.fsmonitor", sc]);
        run(&root, &["config", "log.showSignature", "true"]);
        run(&root, &["config", "gpg.program", sc]);
        run(&root, &["config", "diff.external", sc]);
        run(&root, &["config", "filter.evil.clean", sc]);
        run(&root, &["config", "diff.evil.textconv", sc]);
        std::fs::write(root.join(".gitattributes"), "*.txt diff=evil filter=evil\n").unwrap();
        log(root.clone(), json!({})).await.unwrap();
        list(root.clone(), json!({})).await.unwrap();
        diff_revs_at(&root, &json!({"base": "HEAD~1", "file": "a.txt"}))
            .await
            .unwrap();
        diff_revs_at(&root, &json!({"range": "HEAD~2..HEAD"}))
            .await
            .unwrap();
        assert!(!marker.exists(), "repo-configured program ran");
    }

    #[tokio::test]
    async fn diff_file_must_be_one_changed_path() {
        let (_t, root) = fixture();
        // Tracked secrets below an ordinary directory, changed in a commit.
        std::fs::write(root.join("src/.env"), "TOKEN=1\n").unwrap();
        std::fs::write(root.join("src/credentials.json"), "{}\n").unwrap();
        run(&root, &["add", "-f", "src/.env", "src/credentials.json"]);
        run(&root, &["commit", "-qm", "secrets"]);
        for p in [
            json!({"base": "HEAD~1", "file": "src"}),
            json!({"range": "HEAD~1..HEAD", "file": "src"}),
        ] {
            let e = diff_revs_at(&root, &p).await.unwrap_err();
            assert_eq!(kind(&e), "not_found", "directory selector {p}");
        }
        let v = diff_revs_at(&root, &json!({"range": "HEAD~1..HEAD", "file": "src/.env"}))
            .await
            .unwrap();
        assert_eq!(v["secret"], true);
        assert_eq!(v["diff"], "");
        // Renaming a secret to an innocent name still counts as a secret change.
        run(&root, &["mv", "src/credentials.json", "src/plain.txt"]);
        run(&root, &["commit", "-qm", "rename"]);
        let v = diff_revs_at(
            &root,
            &json!({"range": "HEAD~1..HEAD", "file": "src/plain.txt"}),
        )
        .await
        .unwrap();
        assert_eq!(v["secret"], true);
    }

    #[tokio::test]
    async fn git_dir_is_excluded_case_insensitively() {
        let (_t, root) = fixture();
        for path in [".git/config", ".GIT/config", "x/.Git", ".gIt"] {
            let e = read(root.clone(), json!({"path": path})).await.unwrap_err();
            assert!(
                matches!(kind(&e).as_str(), "permission_denied" | "invalid_params"),
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn diff_base_and_range() {
        let (_t, root) = fixture();
        // base vs working tree, one file.
        let v = diff_revs_at(&root, &json!({"base": "HEAD~2", "file": "a.txt"}))
            .await
            .unwrap();
        let d = v["diff"].as_str().unwrap();
        assert!(d.contains("+two") && d.contains("+three"), "{d}");
        // range, one file: only committed changes.
        let v = diff_revs_at(&root, &json!({"range": "HEAD~2..HEAD~1", "file": "a.txt"}))
            .await
            .unwrap();
        let d = v["diff"].as_str().unwrap();
        assert!(d.contains("+two") && !d.contains("+three"), "{d}");
        // range without a file lists files with counts.
        let v = diff_revs_at(&root, &json!({"range": "HEAD~2..HEAD"}))
            .await
            .unwrap();
        let files: Vec<_> = v["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert_eq!(files, ["a.txt", "src/lib.rs"]);
        assert_eq!(v["files"][0]["adds"], 1);
        // An untracked secret isn't part of the comparison at all.
        let e = diff_revs_at(&root, &json!({"base": "HEAD", "file": ".env"}))
            .await
            .unwrap_err();
        assert_eq!(kind(&e), "not_found");
        for p in [
            json!({"range": "a..b..c", "file": "a.txt"}),
            json!({"range": "--output=x..HEAD"}),
            json!({"base": "-p"}),
            json!({"base": "HEAD", "range": "a..b"}),
            json!({"base": "HEAD", "file": "../x"}),
            json!({"base": "HEAD", "file": "/etc/passwd"}),
        ] {
            let e = diff_revs_at(&root, &p).await.unwrap_err();
            assert_eq!(kind(&e), "invalid_params", "{p}");
        }
    }

    #[tokio::test]
    async fn list_flags_ignored_secret_and_symlinks() {
        let (_t, root) = fixture();
        let v = list(root.clone(), json!({})).await.unwrap();
        assert_eq!(v["path"], "");
        let entries = v["entries"].as_array().unwrap();
        let names: Vec<_> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        // dirs first, then names; .git hidden.
        assert_eq!(
            names,
            [
                "certs",
                "src",
                "target",
                ".env",
                ".gitignore",
                "a.txt",
                "bin.dat",
                "build.log",
                "link.txt",
                "linkdir"
            ]
        );
        let get = |n: &str| entries.iter().find(|e| e["name"] == n).unwrap().clone();
        assert_eq!(get("target")["ignored"], true);
        assert_eq!(get("target")["kind"], "dir");
        assert_eq!(get("build.log")["ignored"], true);
        assert_eq!(get("a.txt")["ignored"], false);
        assert_eq!(get("a.txt")["size"], 14);
        assert_eq!(get(".env")["secret"], true);
        assert!(get(".env").get("size").is_none());
        assert_eq!(get("linkdir")["kind"], "symlink");
        assert_eq!(get("link.txt")["kind"], "symlink");
        let v = list(root.clone(), json!({"path": "src"})).await.unwrap();
        assert_eq!(v["entries"][0]["name"], "lib.rs");
        let v = list(root.clone(), json!({"path": "certs"})).await.unwrap();
        assert_eq!(v["entries"][0]["secret"], true);
        // Traversal and symlinked components.
        for (p, k) in [
            ("..", "invalid_params"),
            ("../outside", "invalid_params"),
            ("/etc", "invalid_params"),
            ("src/../..", "invalid_params"),
            ("linkdir", "permission_denied"),
            (".git", "permission_denied"),
            ("a.txt", "permission_denied"),
        ] {
            let e = list(root.clone(), json!({"path": p})).await.unwrap_err();
            assert_eq!(kind(&e), k, "{p}");
        }
    }

    #[tokio::test]
    async fn list_truncates_at_cap() {
        let (_t, root) = fixture();
        std::fs::create_dir(root.join("many")).unwrap();
        for i in 0..(MAX_ENTRIES + 3) {
            std::fs::write(root.join(format!("many/f{i:05}")), "").unwrap();
        }
        let v = list(root.clone(), json!({"path": "many"})).await.unwrap();
        assert_eq!(v["entries"].as_array().unwrap().len(), MAX_ENTRIES);
        assert_eq!(v["truncated"], true);
    }

    #[tokio::test]
    async fn read_returns_images_on_request() {
        use base64::Engine;
        let (_t, root) = fixture();
        // A real PNG header: the signature, then the IHDR chunk length with its NUL bytes.
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDRrest";
        std::fs::write(root.join("pic.png"), png).unwrap();
        // Without the option an image is plain binary.
        let v = read(root.clone(), json!({"path": "pic.png"}))
            .await
            .unwrap();
        assert_eq!(v["binary"], true);
        assert!(v.get("data_b64").is_none());
        let v = read(root.clone(), json!({"path": "pic.png", "as": "image"}))
            .await
            .unwrap();
        assert_eq!(v["mime"], "image/png");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(v["data_b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(bytes, png);
        // A text file renamed to .png is not served as an image.
        std::fs::write(root.join("fake.png"), "hello").unwrap();
        let v = read(root.clone(), json!({"path": "fake.png", "as": "image"}))
            .await
            .unwrap();
        assert!(v.get("data_b64").is_none());
        assert_eq!(v["text"], "hello");
        // Over the cap: size reported, no data.
        let mut big = png.to_vec();
        big.resize(MAX_IMAGE as usize + 10, 0);
        std::fs::write(root.join("big.png"), &big).unwrap();
        let v = read(root.clone(), json!({"path": "big.png", "as": "image"}))
            .await
            .unwrap();
        assert_eq!(v["truncated"], true);
        assert_eq!(v["size"], big.len());
        assert!(v.get("data_b64").is_none());
        // Symlinks stay refused.
        std::os::unix::fs::symlink(root.join("pic.png"), root.join("l.png")).unwrap();
        assert!(
            read(root.clone(), json!({"path": "l.png", "as": "image"}))
                .await
                .is_err()
        );
        assert_eq!(
            image_mime("a.JPG", &[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(
            image_mime("a.webp", b"RIFF\0\0\0\0WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(image_mime("a.gif", b"GIF89a.."), Some("image/gif"));
        assert_eq!(image_mime("a.svg", b"<svg"), None);
    }

    #[tokio::test]
    async fn read_guards_and_caps() {
        let (_t, root) = fixture();
        let v = read(root.clone(), json!({"path": "a.txt"})).await.unwrap();
        assert_eq!(v["text"], "one\ntwo\nthree\n");
        assert_eq!(
            (v["binary"].clone(), v["truncated"].clone()),
            (json!(false), json!(false))
        );
        assert_eq!(v["size"], 14);
        let v = read(root.clone(), json!({"path": "src/lib.rs"}))
            .await
            .unwrap();
        assert_eq!(v["text"], "fn x() {}\n");
        let v = read(root.clone(), json!({"path": "bin.dat"}))
            .await
            .unwrap();
        assert_eq!(v["binary"], true);
        assert!(v.get("text").is_none());
        for locked in [".env", "certs/server.pem"] {
            let v = read(root.clone(), json!({"path": locked})).await.unwrap();
            assert_eq!(v["secret"], true);
            assert!(v.get("text").is_none());
            let v = read(root.clone(), json!({"path": locked, "as": "image"}))
                .await
                .unwrap();
            assert_eq!(v["secret"], true);
            assert!(v.get("data_b64").is_none());
        }
        // Big file: capped at 512 KiB, multi-byte boundary kept valid.
        let big = "é".repeat(400 * 1024);
        std::fs::write(root.join("big.txt"), &big).unwrap();
        let v = read(root.clone(), json!({"path": "big.txt"}))
            .await
            .unwrap();
        assert_eq!(v["truncated"], true);
        assert_eq!(v["size"], big.len());
        let text = v["text"].as_str().unwrap();
        assert!(text.len() <= MAX_READ as usize && text.len() > MAX_READ as usize - 4);
        assert!(!text.contains('\u{FFFD}'));
        for (p, k) in [
            ("", "invalid_params"),
            ("../outside/x.txt", "invalid_params"),
            ("/etc/hosts", "invalid_params"),
            ("./a.txt", "invalid_params"),
            ("linkdir/x.txt", "permission_denied"),
            ("link.txt", "permission_denied"),
            (".git/config", "permission_denied"),
            ("src", "invalid_params"),
            ("missing.txt", "permission_denied"),
        ] {
            let e = read(root.clone(), json!({"path": p})).await.unwrap_err();
            assert_eq!(kind(&e), k, "{p}");
        }
    }
}

#[derive(Debug, PartialEq)]
struct NameStatus {
    status: char,
    path: String,
    orig: Option<String>,
}

/// Parse `git diff --name-status -z`: `X\0path\0`, or `R100\0old\0new\0` for renames/copies.
fn parse_name_status(out: &[u8]) -> Vec<NameStatus> {
    let mut res = Vec::new();
    let mut it = out
        .split(|&b| b == 0)
        .map(|e| String::from_utf8_lossy(e).to_string());
    while let Some(code) = it.next() {
        let Some(status) = code.chars().next() else {
            continue;
        };
        if matches!(status, 'R' | 'C') {
            let (Some(old), Some(new)) = (it.next(), it.next()) else {
                break;
            };
            res.push(NameStatus {
                status,
                path: new,
                orig: Some(old),
            });
        } else {
            let Some(path) = it.next() else { break };
            res.push(NameStatus {
                status,
                path,
                orig: None,
            });
        }
    }
    res
}

#[cfg(test)]
mod name_status_tests {
    use super::*;

    #[test]
    fn parses_name_status() {
        let v = parse_name_status(b"M\0src/a.rs\0A\0new.rs\0R087\0old.rs\0moved.rs\0D\0gone.rs\0");
        assert_eq!(
            v[0],
            NameStatus {
                status: 'M',
                path: "src/a.rs".into(),
                orig: None
            }
        );
        assert_eq!(v[1].status, 'A');
        assert_eq!(
            v[2],
            NameStatus {
                status: 'R',
                path: "moved.rs".into(),
                orig: Some("old.rs".into())
            }
        );
        assert_eq!(
            v[3],
            NameStatus {
                status: 'D',
                path: "gone.rs".into(),
                orig: None
            }
        );
    }
}
