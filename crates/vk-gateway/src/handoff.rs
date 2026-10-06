//! Handoff (spec 16 §15.2): export an agent's work at a turn boundary as a bundle, carry it
//! through the app, import it on another host as a new worktree and resume the agent there.
//!
//! The bundle is a zstd-compressed tar: `manifest.json`, `repo.bundle` (optional), `changes.patch`,
//! `untracked/<path>` and `transcript.jsonl` (optional).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::Gateway;
use crate::api::{ApiError, ApiResult, normalize};
use crate::state::{Device, now_s};

const MAX_BUNDLE: u64 = 200 * 1024 * 1024;
const MAX_CHUNK: u64 = 4 * 1024 * 1024;
const MAX_UNTRACKED: u64 = 5 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(3600);

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

enum Dir {
    Out,
    In {
        expected_size: u64,
        sha256: String,
        manifest: Box<Manifest>,
    },
}

struct Entry {
    owner: String,
    path: PathBuf,
    size: u64,
    created: Instant,
    dir: Dir,
}

static ENTRIES: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

fn with_entries<T>(f: impl FnOnce(&mut HashMap<String, Entry>) -> T) -> T {
    let mut g = ENTRIES.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    m.retain(|_, e| {
        let keep = e.created.elapsed() < TTL;
        if !keep {
            let _ = std::fs::remove_file(&e.path);
        }
        keep
    });
    f(m)
}

/// Side effects re-check that the device is still authorized (spec 16 §4.6).
fn still_authorized(gw: &Gateway, dev: &Device) -> Result<(), ApiError> {
    let _ = gw.reload_devices();
    match gw.device(&dev.id) {
        Some(d) if !d.expired() => Ok(()),
        _ => Err(err("forbidden", "this device is no longer authorized")),
    }
}

fn err(kind: &str, m: impl Into<String>) -> ApiError {
    ApiError::new(kind, m)
}

fn s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(|v| v.as_str())
}

pub async fn dispatch(gw: &Arc<Gateway>, dev: &Device, method: &str, p: &Value) -> ApiResult {
    match method {
        "handoff.export" => export(gw, dev, p).await,
        "handoff.read" => read(dev, p),
        "handoff.discard" => {
            let id = s(p, "id").unwrap_or("");
            with_entries(|m| {
                if m.get(id).is_some_and(|e| e.owner == dev.id)
                    && let Some(e) = m.remove(id)
                {
                    let _ = std::fs::remove_file(e.path);
                }
            });
            Ok(json!({}))
        }
        "handoff.begin" => begin(gw, dev, p),
        "handoff.write" => write(dev, p),
        "handoff.finish" => finish(gw, dev, p).await,
        _ => Err(err("method_not_found", method)),
    }
}

fn dir(gw: &Gateway) -> Result<PathBuf, ApiError> {
    let d = gw.state.dir.join("handoffs");
    std::fs::create_dir_all(&d).map_err(|e| err("internal", e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
    }
    Ok(d)
}

// ---------------------------------------------------------------------------------------------
// git

/// Run git on this host. Same hardening as the server's git methods: repo-configured programs
/// never run (spec 16 §7.7).
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

async fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, ApiError> {
    let filters = filter_overrides(dir).await;
    let out = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new("git")
            .arg("--literal-pathspecs")
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
        err(
            "timeout",
            format!("git {} timed out", args.first().unwrap_or(&"")),
        )
    })?
    .map_err(|e| err("unsupported", format!("git: {e}")))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(err(
            "conflict",
            format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ))
    }
}

async fn git_line(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args)
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn secret_path(path: &str) -> bool {
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

fn safe_relative(path: &str) -> bool {
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
fn regular_under(root: &Path, rel: &str) -> Option<std::fs::Metadata> {
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

// ---------------------------------------------------------------------------------------------
// export

async fn export(gw: &Arc<Gateway>, dev: &Device, p: &Value) -> ApiResult {
    let pane = s(p, "pane").ok_or_else(|| ApiError::invalid("pane is required"))?;
    let info = gw.server.call("pane.get", json!({"pane": pane})).await?;
    let cwd = s(&info, "cwd")
        .map(PathBuf::from)
        .ok_or_else(|| err("not_found", "pane has no working directory"))?;
    let mut run = info.get("run").cloned().unwrap_or(Value::Null);
    normalize(&mut run);

    // Turn boundary: the agent must be idle.
    if run.is_object() {
        let state = run
            .pointer("/execution/value")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        if matches!(state, "working" | "starting") {
            if p.get("interrupt").and_then(|v| v.as_bool()) != Some(true) {
                return Err(err(
                    "busy",
                    "the agent is working; wait for it to finish or pass interrupt: true",
                ));
            }
            let id = s(&run, "id").unwrap_or("").to_string();
            still_authorized(gw, dev)?;
            gw.server
                .call_as(
                    &format!("gateway:{}", dev.name),
                    "agent.interrupt",
                    json!({"target": id}),
                )
                .await?;
            let r = gw.server.call("agent.wait", json!({"target": id, "until": ["idle", "exited", "error"], "timeout_ms": 30_000})).await;
            if r.is_err() {
                return Err(err("timeout", "the agent did not stop within 30 s"));
            }
        }
    }

    let root = git_line(&cwd, &["rev-parse", "--show-toplevel"])
        .await
        .map(PathBuf::from)
        .ok_or_else(|| err("not_found", "not_a_repo"))?;
    let head = git_line(&root, &["rev-parse", "HEAD"])
        .await
        .ok_or_else(|| err("conflict", "repository has no commits"))?;
    let branch = git_line(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .filter(|b| b != "HEAD");
    let origin = git_line(&root, &["remote", "get-url", "origin"]).await;
    let has_remotes = git_line(&root, &["remote"]).await.is_some();
    let full = p.get("full").and_then(|v| v.as_bool()) == Some(true);

    let id = ulid::Ulid::new().to_string().to_lowercase();
    let work = tempfile::Builder::new()
        .prefix("handoff-")
        .tempdir_in(dir(gw)?)
        .map_err(|e| err("internal", e.to_string()))?;
    let w = work.path().to_path_buf();

    // Repository objects.
    let bundle_path = w.join("repo.bundle");
    let bundle_kind = if has_remotes && !full {
        match git(
            &root,
            &[
                "bundle",
                "create",
                bundle_path.to_str().unwrap_or_default(),
                "HEAD",
                "--not",
                "--remotes",
            ],
        )
        .await
        {
            Ok(_) => "thin",
            Err(e) if e.message.contains("empty bundle") => "none",
            Err(e) => return Err(e),
        }
    } else {
        git(
            &root,
            &[
                "bundle",
                "create",
                bundle_path.to_str().unwrap_or_default(),
                "HEAD",
            ],
        )
        .await?;
        "full"
    };
    if (std::fs::metadata(&bundle_path)
        .map(|m| m.len())
        .unwrap_or(0))
        > MAX_BUNDLE
    {
        return Err(err("too_large", "repository bundle exceeds 200 MiB"));
    }

    // Uncommitted tracked changes.
    let patch = git(
        &root,
        &["diff", "--binary", "--no-ext-diff", "--no-textconv", "HEAD"],
    )
    .await?;
    std::fs::write(w.join("changes.patch"), &patch).map_err(|e| err("internal", e.to_string()))?;

    // Untracked files.
    let listed = git(&root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    let mut untracked = Vec::new();
    let mut skipped = Vec::new();
    for raw in listed.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let rel = String::from_utf8_lossy(raw).to_string();
        let reason = if !safe_relative(&rel) {
            Some("unsafe path")
        } else if secret_path(&rel) {
            Some("secret")
        } else {
            match regular_under(&root, &rel) {
                None => Some("not a regular file"),
                Some(md) if md.len() > MAX_UNTRACKED => Some("larger than 5 MiB"),
                Some(_) => None,
            }
        };
        match reason {
            Some(r) => skipped.push(Skipped {
                path: rel,
                reason: r.into(),
            }),
            None => untracked.push(rel),
        }
    }

    // Transcript, redacted line by line.
    let (harness, session_id) = (
        s(&run, "harness").map(str::to_string),
        s(&run, "harness_session_id").map(str::to_string),
    );
    let resume_args: Vec<String> = run
        .get("resume_argv")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .skip(1)
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut redactions = 0;
    let mut transcript_rel = None;
    if let Some(tp) = s(&run, "transcript_path") {
        let rel = transcript_relative(harness.as_deref().unwrap_or(""), tp);
        if let (Some(rel), Ok(text)) = (rel, std::fs::read_to_string(tp)) {
            let mut out = String::with_capacity(text.len());
            for line in text.lines() {
                // Keep untouched lines byte-for-byte; only rewrite lines that had secrets.
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
            std::fs::write(w.join("transcript.jsonl"), out)
                .map_err(|e| err("internal", e.to_string()))?;
            transcript_rel = Some(rel);
        }
    }

    let cwd_rel = cwd
        .strip_prefix(&root)
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let manifest = Manifest {
        v: 1,
        source_host: gw.host_name.clone(),
        repo_name: root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".into()),
        origin,
        branch,
        head,
        bundle: bundle_kind.into(),
        cwd_rel,
        source_cwd: cwd.display().to_string(),
        source_root: root.display().to_string(),
        harness,
        session_id,
        resume_args,
        transcript_rel,
        last_message: s(&run, "last_message").map(|m| m.chars().take(500).collect()),
        untracked: untracked.clone(),
        skipped,
        redactions,
        created_at: now_s(),
    };

    // Pack.
    let out_path = dir(gw)?.join(format!("{id}.out.tar.zst"));
    let (root2, w2, m2, out2) = (root.clone(), w.clone(), manifest.clone(), out_path.clone());
    let bundle_present = bundle_kind != "none";
    tokio::task::spawn_blocking(move || pack(&out2, &w2, &root2, &m2, bundle_present))
        .await
        .map_err(|e| err("internal", e.to_string()))?
        .map_err(|e| err("internal", e.to_string()))?;
    let (size, sha) = hash_file(&out_path).map_err(|e| err("internal", e.to_string()))?;
    if size > MAX_BUNDLE {
        let _ = std::fs::remove_file(&out_path);
        return Err(err("too_large", "handoff bundle exceeds 200 MiB"));
    }
    with_entries(|m| {
        m.insert(
            id.clone(),
            Entry {
                owner: dev.id.clone(),
                path: out_path,
                size,
                created: Instant::now(),
                dir: Dir::Out,
            },
        )
    });
    gw.state.audit(&json!({"ts": now_s(), "event": "handoff.exported", "device": dev.id, "pane": pane, "size": size}));
    Ok(json!({"id": id, "size": size, "sha256": sha, "manifest": manifest}))
}

fn transcript_relative(harness: &str, path: &str) -> Option<String> {
    let marker = match harness {
        "claude" => "/projects/",
        "codex" => "/sessions/",
        _ => return None,
    };
    path.rfind(marker).map(|i| path[i + 1..].to_string())
}

fn pack(out: &Path, work: &Path, root: &Path, m: &Manifest, bundle: bool) -> std::io::Result<()> {
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

fn hash_file(p: &Path) -> std::io::Result<(u64, String)> {
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

// ---------------------------------------------------------------------------------------------
// transfer

fn read(dev: &Device, p: &Value) -> ApiResult {
    let id = s(p, "id").unwrap_or("");
    let offset = p.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
    let len = p
        .get("len")
        .and_then(|v| v.as_u64())
        .unwrap_or(MAX_CHUNK)
        .min(MAX_CHUNK);
    let (path, size) = with_entries(|m| match m.get(id) {
        Some(e) if e.owner == dev.id && matches!(e.dir, Dir::Out) => Ok((e.path.clone(), e.size)),
        _ => Err(err("not_found", "no such handoff")),
    })?;
    let mut f = std::fs::File::open(&path).map_err(|e| err("internal", e.to_string()))?;
    f.seek(SeekFrom::Start(offset.min(size)))
        .map_err(|e| err("internal", e.to_string()))?;
    let mut buf = Vec::new();
    f.take(len)
        .read_to_end(&mut buf)
        .map_err(|e| err("internal", e.to_string()))?;
    let eof = offset + buf.len() as u64 >= size;
    Ok(json!({"data_b64": B64.encode(&buf), "eof": eof, "size": size}))
}

fn begin(gw: &Gateway, dev: &Device, p: &Value) -> ApiResult {
    let manifest: Manifest = serde_json::from_value(p.get("manifest").cloned().unwrap_or_default())
        .map_err(|e| ApiError::invalid(format!("manifest: {e}")))?;
    let size = p
        .get("size")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| ApiError::invalid("size"))?;
    let sha = s(p, "sha256")
        .filter(|h| h.len() == 64)
        .ok_or_else(|| ApiError::invalid("sha256"))?
        .to_string();
    if size > MAX_BUNDLE {
        return Err(err("too_large", "handoff bundle exceeds 200 MiB"));
    }
    // Per sender, so one device's abandoned transfers can't block everyone (plus a global cap).
    let (mine, all) = with_entries(|m| {
        let inc = m.values().filter(|e| matches!(e.dir, Dir::In { .. }));
        let all: Vec<&Entry> = inc.collect();
        (all.iter().filter(|e| e.owner == dev.id).count(), all.len())
    });
    if mine >= 2 || all >= 8 {
        return Err(err("rate_limited", "too many handoffs in progress"));
    }
    let id = ulid::Ulid::new().to_string().to_lowercase();
    let path = dir(gw)?.join(format!("{id}.in.tar.zst"));
    std::fs::File::create(&path).map_err(|e| err("internal", e.to_string()))?;
    with_entries(|m| {
        m.insert(
            id.clone(),
            Entry {
                owner: dev.id.clone(),
                path,
                size: 0,
                created: Instant::now(),
                dir: Dir::In {
                    expected_size: size,
                    sha256: sha,
                    manifest: Box::new(manifest),
                },
            },
        )
    });
    Ok(json!({"id": id}))
}

fn write(dev: &Device, p: &Value) -> ApiResult {
    let id = s(p, "id").unwrap_or("");
    let offset = p
        .get("offset")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| ApiError::invalid("offset"))?;
    let data = B64
        .decode(s(p, "data_b64").unwrap_or(""))
        .map_err(|_| ApiError::invalid("data_b64"))?;
    if data.len() as u64 > MAX_CHUNK {
        return Err(err("too_large", "chunks are at most 4 MiB"));
    }
    with_entries(|m| {
        let e = m
            .get_mut(id)
            .filter(|e| e.owner == dev.id)
            .ok_or_else(|| err("not_found", "no such handoff"))?;
        let Dir::In { expected_size, .. } = &e.dir else {
            return Err(err("not_found", "no such handoff"));
        };
        if offset != e.size {
            return Err(err("conflict", format!("expected offset {}", e.size)));
        }
        if e.size + data.len() as u64 > *expected_size {
            return Err(err("too_large", "more data than announced"));
        }
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&e.path)
            .map_err(|x| err("internal", x.to_string()))?;
        f.write_all(&data)
            .map_err(|x| err("internal", x.to_string()))?;
        e.size += data.len() as u64;
        Ok(json!({"received": e.size}))
    })
}

// ---------------------------------------------------------------------------------------------
// import

async fn finish(gw: &Arc<Gateway>, dev: &Device, p: &Value) -> ApiResult {
    let id = s(p, "id").unwrap_or("").to_string();
    let (path, manifest) = with_entries(|m| {
        let e = m
            .get(&id)
            .filter(|e| e.owner == dev.id)
            .ok_or_else(|| err("not_found", "no such handoff"))?;
        let Dir::In {
            expected_size,
            sha256,
            manifest,
        } = &e.dir
        else {
            return Err(err("not_found", "no such handoff"));
        };
        if e.size != *expected_size {
            return Err(err(
                "conflict",
                format!("received {} of {} bytes", e.size, expected_size),
            ));
        }
        Ok((e.path.clone(), (manifest.clone(), sha256.clone())))
    })?;
    let (manifest, sha) = manifest;
    let (_, got) = hash_file(&path).map_err(|e| err("internal", e.to_string()))?;
    if got != sha {
        return Err(err("conflict", "checksum mismatch; send the handoff again"));
    }

    // Unpack (regular files only, relative paths only).
    let work = tempfile::Builder::new()
        .prefix("import-")
        .tempdir_in(dir(gw)?)
        .map_err(|e| err("internal", e.to_string()))?;
    let w = work.path().to_path_buf();
    let p2 = path.clone();
    let packed: Manifest = tokio::task::spawn_blocking(move || unpack(&p2, &w))
        .await
        .map_err(|e| err("internal", e.to_string()))?
        .map_err(|e| err("invalid_params", format!("bad bundle: {e}")))?;
    // The manifest the user reviewed must be exactly the one inside the bundle.
    if serde_json::to_value(&packed).ok() != serde_json::to_value(&*manifest).ok() {
        return Err(err("conflict", "manifest does not match the bundle"));
    }
    if packed.head.len() != 40 || !packed.head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::invalid("head must be a full commit id"));
    }
    let w = work.path();
    if packed.bundle != "none" {
        // The bundle's advertised HEAD must be that commit.
        let heads = git(
            w,
            &[
                "bundle",
                "list-heads",
                w.join("repo.bundle").to_str().unwrap_or_default(),
            ],
        )
        .await?;
        if !String::from_utf8_lossy(&heads)
            .lines()
            .any(|l| l.starts_with(&packed.head))
        {
            return Err(err("conflict", "bundle HEAD does not match the manifest"));
        }
    }

    // Find the repository.
    if dev.kind == "handoff" && s(p, "repo_path").is_some() {
        return Err(err(
            "forbidden",
            "a handoff invitation cannot choose where work lands",
        ));
    }
    let root = match s(p, "repo_path") {
        Some(rp) => git_line(&expand_home(rp), &["rev-parse", "--show-toplevel"])
            .await
            .map(PathBuf::from)
            .ok_or_else(|| err("not_found", "repo_path is not a git repository"))?,
        None => find_repo(gw, &packed).await.ok_or_else(|| ApiError {
            kind: "needs_repo".into(),
            message: format!(
                "no local clone of {} found; pass repo_path",
                packed.origin.as_deref().unwrap_or(&packed.repo_name)
            ),
            details: json!({"origin": packed.origin, "repo_name": packed.repo_name}),
        })?,
    };

    // Objects → commit.
    let commit = packed.head.clone();
    if packed.bundle != "none" {
        let b = w.join("repo.bundle");
        let bs = b.to_str().unwrap_or_default();
        if git(&root, &["fetch", "--no-tags", bs, "HEAD"])
            .await
            .is_err()
        {
            git(&root, &["fetch", "--no-tags", "origin"]).await?;
            git(&root, &["fetch", "--no-tags", bs, "HEAD"]).await?;
        }
    } else if git(&root, &["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .await
        .is_err()
    {
        git(&root, &["fetch", "--no-tags", "origin"]).await?;
    }
    git(&root, &["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .await
        .map_err(|_| err("conflict", "the handed-off commit is not available"))?;

    // Worktree on a new branch.
    let base_branch = packed.branch.clone().unwrap_or_else(|| "detached".into());
    let slug: String = base_branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let parent = root.parent().unwrap_or(&root).to_path_buf();
    let repo_name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into());
    let (mut wt, mut br) = (
        parent.join(format!("{repo_name}-handoff-{slug}")),
        format!("handoff/{base_branch}"),
    );
    for n in 2..100 {
        let exists = git(
            &root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{br}"),
            ],
        )
        .await
        .is_ok();
        if !wt.exists() && !exists {
            break;
        }
        wt = parent.join(format!("{repo_name}-handoff-{slug}-{n}"));
        br = format!("handoff/{base_branch}-{n}");
    }
    git(
        &root,
        &[
            "worktree",
            "add",
            "-b",
            &br,
            wt.to_str().unwrap_or_default(),
            &commit,
        ],
    )
    .await?;

    let patch = w.join("changes.patch");
    if std::fs::metadata(&patch).map(|m| m.len()).unwrap_or(0) > 0 {
        git(
            &wt,
            &[
                "apply",
                "--binary",
                "--whitespace=nowarn",
                patch.to_str().unwrap_or_default(),
            ],
        )
        .await?;
    }
    for rel in &packed.untracked {
        if !safe_relative(rel) {
            continue;
        }
        // The patch may have created symlinks: never write through one.
        if let Err(e) = write_new_file(&wt, rel, &w.join("untracked").join(rel)) {
            tracing::warn!("handoff: skipped {rel}: {e}");
        }
    }
    if !packed.cwd_rel.is_empty() && !safe_relative(&packed.cwd_rel) {
        return Err(ApiError::invalid(
            "manifest cwd is not a relative path inside the repository",
        ));
    }

    // Transcript → where the harness resumes from, with paths rewritten.
    // The cwd must resolve inside the worktree (the patch could have made it a symlink).
    let new_cwd = match wt.join(&packed.cwd_rel).canonicalize() {
        Ok(c)
            if !packed.cwd_rel.is_empty()
                && c.is_dir()
                && wt.canonicalize().is_ok_and(|w| c.starts_with(&w)) =>
        {
            c
        }
        _ => wt.clone(),
    };
    let resumed = match install_transcript(&packed, w, &new_cwd, &wt) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("handoff transcript: {e}");
            false
        }
    };

    // Workspace + agent.
    still_authorized(gw, dev)?;
    let ws = gw
        .server
        .call_as(
            &format!("gateway:{}", dev.name),
            "workspace.create",
            json!({"cwd": new_cwd}),
        )
        .await?;
    let pane = ws
        .pointer("/root_pane/id")
        .or_else(|| ws.get("root_pane"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let mut result = json!({"workspace": ws.pointer("/workspace/id"), "pane": pane, "worktree": wt, "branch": br, "resumed": resumed,
                            "skipped": packed.skipped});
    // A teammate's invitation stages the work; only the host's owner starts agents on it.
    let teammate = dev.kind == "handoff";
    let start = !teammate
        && p.get("start_agent")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
    if teammate {
        let _ = gw
            .server
            .call_as(
                &format!("gateway:{}", dev.name),
                "notification.send",
                json!({"title": format!("Handoff from {}", clean(&packed.source_host, 60)),
                       "body": format!("Branch {br} is ready in {}{}", wt.display(),
                                       if resumed { " (resumable session)" } else { "" }),
                       "urgency": "normal", "pane": pane}),
            )
            .await;
        result["staged"] = true.into();
    }
    if let (true, Some(pane), Some(harness)) = (
        start,
        pane,
        packed.harness.clone().filter(|h| known_harness(h)),
    ) {
        let note = format!(
            "This session was handed off from {} ({}). The work continues in {} on branch {}. Files not carried over: {}.",
            clean(&packed.source_host, 60),
            clean(&packed.source_cwd, 200),
            new_cwd.display(),
            br,
            if packed.skipped.is_empty() {
                "none".to_string()
            } else {
                packed
                    .skipped
                    .iter()
                    .take(20)
                    .map(|s| clean(&s.path, 120))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );
        let mut params = json!({"pane": pane, "harness": harness, "prompt": note});
        // Rebuilt from the harness and a validated session id; never the manifest's argv.
        if resumed && let Some(args) = resume_args(&harness, packed.session_id.as_deref()) {
            params["args"] = json!(args);
        }
        still_authorized(gw, dev)?;
        match gw
            .server
            .call_as(&format!("gateway:{}", dev.name), "agent.start", params)
            .await
        {
            Ok(r) => result["run"] = r.get("run").cloned().unwrap_or(r),
            Err(e) => result["agent_error"] = e.to_json(),
        }
    }
    with_entries(|m| {
        if let Some(e) = m.remove(&id) {
            let _ = std::fs::remove_file(e.path);
        }
    });
    gw.state.audit(&json!({"ts": now_s(), "event": "handoff.imported", "device": dev.id, "from": packed.source_host, "worktree": wt}));
    Ok(result)
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

fn unpack(bundle: &Path, out: &Path) -> std::io::Result<Manifest> {
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

async fn find_repo(gw: &Gateway, m: &Manifest) -> Option<PathBuf> {
    let origin = m.origin.as_deref()?;
    let snap = gw.server.call("session.snapshot", json!({})).await.ok()?;
    let mut seen = Vec::new();
    for w in snap
        .get("workspaces")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let Some(root) = s(w, "root_path") else {
            continue;
        };
        let Some(top) = git_line(Path::new(root), &["rev-parse", "--show-toplevel"]).await else {
            continue;
        };
        if seen.contains(&top) {
            continue;
        }
        seen.push(top.clone());
        if git_line(Path::new(&top), &["remote", "get-url", "origin"])
            .await
            .is_some_and(|o| same_remote(&o, origin))
        {
            return Some(PathBuf::from(top));
        }
    }
    None
}

/// `git@github.com:a/b.git` ≡ `https://github.com/a/b`.
/// `(host, path)` of a git remote: URL form `scheme://[user@]host[:port]/path` or scp form
/// `[user@]host:path`. Host is case-insensitive; the path is not.
fn remote_parts(u: &str) -> Option<(String, String)> {
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

fn same_remote(a: &str, b: &str) -> bool {
    matches!((remote_parts(a), remote_parts(b)), (Some(x), Some(y)) if x == y)
}

fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ if p == "~" => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default(),
        _ => PathBuf::from(p),
    }
}

fn harness_home(harness: &str) -> Option<PathBuf> {
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

/// Returns whether the harness can resume from the installed transcript.
fn install_transcript(
    m: &Manifest,
    work: &Path,
    new_cwd: &Path,
    new_root: &Path,
) -> std::io::Result<bool> {
    let src = work.join("transcript.jsonl");
    let (Some(h), Some(rel), true) = (
        m.harness.as_deref(),
        m.transcript_rel.as_deref(),
        src.exists(),
    ) else {
        return Ok(false);
    };
    let Some(session) = m.session_id.as_deref().filter(|id| valid_session_id(id)) else {
        return Ok(false);
    };
    if resume_args(h, Some(session)).is_none() {
        return Ok(false);
    }
    let Some(home) = harness_home(h) else {
        return Ok(false);
    };
    let raw = std::fs::read_to_string(&src)?;
    if raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .any(|l| serde_json::from_str::<Value>(l).is_err())
    {
        return Err(std::io::Error::other("transcript is not JSON lines"));
    }
    let text = raw
        .replace(&m.source_cwd, &new_cwd.display().to_string())
        .replace(&m.source_root, &new_root.display().to_string());
    let file = Path::new(rel)
        .file_name()
        .ok_or_else(|| std::io::Error::other("bad transcript path"))?;
    let dest = match h {
        "claude" => home
            .join("projects")
            .join(claude_project_dir(new_cwd))
            .join(format!("{session}.jsonl")),
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
            home.join(rel)
        }
        _ => return Ok(false),
    };
    if dest.exists() {
        return Err(std::io::Error::other(format!(
            "{} already exists",
            dest.display()
        )));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&dest)?
        .write_all(text.as_bytes())?;
    Ok(true)
}

fn known_harness(h: &str) -> bool {
    matches!(h, "claude" | "codex" | "pi" | "omp")
}

fn valid_session_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Resume argv for a harness, built locally (spec 16 §15.2).
fn resume_args(harness: &str, session: Option<&str>) -> Option<Vec<String>> {
    let id = session.filter(|id| valid_session_id(id))?;
    match harness {
        "claude" => Some(vec!["--resume".into(), id.into()]),
        "codex" => Some(vec!["resume".into(), id.into()]),
        _ => None,
    }
}

/// Text from a manifest shown to people or agents: no control characters, bounded length.
fn clean(s: &str, max: usize) -> String {
    s.chars().filter(|c| !c.is_control()).take(max).collect()
}

/// Create `root/rel` from `src` without following symlinks in any existing component and without
/// replacing anything.
fn write_new_file(root: &Path, rel: &str, src: &Path) -> std::io::Result<()> {
    let mut p = root.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for d in &parts[..parts.len() - 1] {
        p.push(d);
        match std::fs::symlink_metadata(&p) {
            Ok(md) if md.file_type().is_symlink() || !md.is_dir() => {
                return Err(std::io::Error::other("path crosses a symlink or file"));
            }
            Ok(_) => {}
            Err(_) => std::fs::create_dir(&p)?,
        }
    }
    use std::os::unix::fs::OpenOptionsExt;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join(rel))?;
    std::io::copy(&mut std::fs::File::open(src)?, &mut out)?;
    Ok(())
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
}

#[cfg(test)]
mod hardening_tests {
    use super::*;

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
    fn codex_transcript_stays_in_sessions() {
        let t = tempfile::tempdir().unwrap();
        let work = t.path().join("w");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("transcript.jsonl"), "{}\n").unwrap();
        // SAFETY: test-local env; only this test reads CODEX_HOME.
        unsafe { std::env::set_var("CODEX_HOME", t.path().join("codex")) };
        let mut m = Manifest {
            harness: Some("codex".into()),
            session_id: Some("s1".into()),
            ..Default::default()
        };
        m.transcript_rel = Some("config.toml".into());
        assert!(install_transcript(&m, &work, t.path(), t.path()).is_err());
        m.transcript_rel = Some("sessions/../config.toml".into());
        assert!(install_transcript(&m, &work, t.path(), t.path()).is_err());
        m.transcript_rel = Some("sessions/2026/10/06/rollout-x-s1.jsonl".into());
        assert!(install_transcript(&m, &work, t.path(), t.path()).unwrap());
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
        write_new_file(&root, "a/b/c.txt", &src).unwrap();
        assert!(
            write_new_file(&root, "a/b/c.txt", &src).is_err(),
            "never replaces"
        );
    }
}

#[cfg(test)]
mod remote_tests {
    use super::*;

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
}
