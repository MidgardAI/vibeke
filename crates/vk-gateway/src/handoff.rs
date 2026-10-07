//! Handoff (spec 16 §15.2): export an agent's work at a turn boundary as a bundle, carry it
//! through the app and deliver it to this host's server as an incoming handoff, which the receiver
//! accepts (or the server imports automatically) as a new worktree with the agent resumed there.
//!
//! Bundle format, unpacking and the repository-side import live in `vk-handoff`; the import and
//! the incoming records live in the server (`handoff.incoming.*`, `handoff.accept`). This module
//! does the export, the transfer and the delivery.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use vk_handoff::{
    MAX_BUNDLE, MAX_UNTRACKED, Manifest, Skipped, clean, expand_home, git, git_line, hash_file,
    regular_under, safe_relative, secret_path,
};

pub use vk_handoff::claude_project_dir;

use crate::Gateway;
use crate::api::{ApiError, ApiResult, normalize};
use crate::state::{Device, now_s};

const MAX_CHUNK: u64 = 4 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(3600);

impl From<vk_handoff::Error> for ApiError {
    fn from(e: vk_handoff::Error) -> Self {
        ApiError::new(e.kind, e.message)
    }
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
    /// A chunk write or the import is running outside the lock.
    busy: bool,
}

static ENTRIES: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

/// Runs `f` under the lock; files of expired entries are removed after it is released.
fn with_entries<T>(f: impl FnOnce(&mut HashMap<String, Entry>) -> T) -> T {
    let mut expired = Vec::new();
    let r = {
        let mut g = ENTRIES.lock().unwrap();
        let m = g.get_or_insert_with(HashMap::new);
        m.retain(|_, e| {
            let keep = e.busy || e.created.elapsed() < TTL;
            if !keep {
                expired.push(e.path.clone());
            }
            keep
        });
        f(m)
    };
    for p in expired {
        let _ = std::fs::remove_file(p);
    }
    r
}

fn remove_entry(id: &str, owner: &str) {
    let e = with_entries(|m| {
        if m.get(id).is_some_and(|e| e.owner == owner) {
            m.remove(id)
        } else {
            None
        }
    });
    if let Some(e) = e {
        let _ = std::fs::remove_file(e.path);
    }
}

/// Remove files in the handoffs directory that no entry refers to. Entries live in memory, so at
/// startup everything there is left over from before a restart.
pub fn sweep(gw: &Gateway) {
    let Ok(d) = dir(gw) else { return };
    let live: Vec<PathBuf> = with_entries(|m| m.values().map(|e| e.path.clone()).collect());
    for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
        let p = e.path();
        if live.contains(&p) {
            continue;
        }
        let r = match e.file_type() {
            Ok(t) if t.is_dir() => std::fs::remove_dir_all(&p),
            _ => std::fs::remove_file(&p),
        };
        if let Err(x) = r {
            tracing::warn!("handoff sweep: {}: {x}", p.display());
        }
    }
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

/// Disk work off the async threads.
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| err("internal", e.to_string()))?
        .map_err(|e| err("internal", e.to_string()))
}

pub async fn dispatch(gw: &Arc<Gateway>, dev: &Device, method: &str, p: &Value) -> ApiResult {
    match method {
        "handoff.export" => export(gw, dev, p).await,
        "handoff.read" => read(dev, p).await,
        "handoff.discard" => {
            remove_entry(s(p, "id").unwrap_or(""), &dev.id);
            Ok(json!({}))
        }
        "handoff.begin" => begin(gw, dev, p),
        "handoff.write" => write(dev, p).await,
        "handoff.finish" => finish(gw, dev, p).await,
        "handoff.incoming.list"
        | "handoff.incoming.get"
        | "handoff.accept"
        | "handoff.decline"
        | "handoff.resume"
        | "handoff.prefs" => incoming(gw, dev, method, p).await,
        _ => Err(err("method_not_found", method)),
    }
}

pub(crate) fn dir(gw: &Gateway) -> Result<PathBuf, ApiError> {
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
// export

async fn export(gw: &Arc<Gateway>, dev: &Device, p: &Value) -> ApiResult {
    let pane = s(p, "pane").ok_or_else(|| ApiError::invalid("pane is required"))?;
    let ex = export_bundle(
        gw,
        &format!("gateway:{}", dev.name),
        Some(dev),
        pane,
        p.get("interrupt").and_then(|v| v.as_bool()) == Some(true),
        p.get("full").and_then(|v| v.as_bool()) == Some(true),
        None,
    )
    .await?;
    with_entries(|m| {
        m.insert(
            ex.id.clone(),
            Entry {
                owner: dev.id.clone(),
                path: ex.path.clone(),
                size: ex.size,
                created: Instant::now(),
                dir: Dir::Out,
                busy: false,
            },
        )
    });
    gw.state.audit(&json!({"ts": now_s(), "event": "handoff.exported", "device": dev.id, "pane": pane, "size": ex.size}));
    Ok(json!({"id": ex.id, "size": ex.size, "sha256": ex.sha256, "manifest": ex.manifest}))
}

/// A packed bundle in the handoffs directory (`<id>.out.tar.zst`); the caller owns the file.
#[derive(Debug, Clone)]
pub struct Exported {
    pub id: String,
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub manifest: Manifest,
}

/// Export the work in `pane` at a turn boundary. `actor` names who asks (server audit);
/// `auth`, when a device asks, is re-authorized before its agent is interrupted. Without a
/// device (a server job, `handoff.send`) the server already authorized the request. A job's id
/// goes into the manifest as `source_job`.
pub async fn export_bundle(
    gw: &Arc<Gateway>,
    actor: &str,
    auth: Option<&Device>,
    pane: &str,
    interrupt: bool,
    full: bool,
    source_job: Option<&str>,
) -> Result<Exported, ApiError> {
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
            if !interrupt {
                return Err(err(
                    "busy",
                    "the agent is working; wait for it to finish or pass interrupt: true",
                ));
            }
            let id = s(&run, "id").unwrap_or("").to_string();
            if let Some(dev) = auth {
                still_authorized(gw, dev)?;
            }
            gw.server
                .call_as(actor, "agent.interrupt", json!({"target": id}))
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
            Err(e) => return Err(e.into()),
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

    // Transcript (and Claude's sidechain files), redacted line by line.
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
        let (h, tp, w2) = (
            harness.clone().unwrap_or_default(),
            PathBuf::from(tp),
            w.clone(),
        );
        if let Some((rel, n)) =
            blocking(move || vk_handoff::export_transcript(&h, &tp, &w2)).await?
        {
            transcript_rel = Some(rel);
            redactions = n;
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
        source_job: source_job.map(str::to_string),
    };

    // Pack.
    let out_path = dir(gw)?.join(format!("{id}.out.tar.zst"));
    let (root2, w2, m2, out2) = (root.clone(), w.clone(), manifest.clone(), out_path.clone());
    let bundle_present = bundle_kind != "none";
    let (size, sha) = blocking(move || {
        vk_handoff::pack(&out2, &w2, &root2, &m2, bundle_present)?;
        hash_file(&out2)
    })
    .await?;
    if size > MAX_BUNDLE {
        let _ = std::fs::remove_file(&out_path);
        return Err(err("too_large", "handoff bundle exceeds 200 MiB"));
    }
    Ok(Exported {
        id,
        path: out_path,
        size,
        sha256: sha,
        manifest,
    })
}

// ---------------------------------------------------------------------------------------------
// transfer

async fn read(dev: &Device, p: &Value) -> ApiResult {
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
    let buf = blocking(move || {
        let mut f = std::fs::File::open(&path)?;
        f.seek(SeekFrom::Start(offset.min(size)))?;
        let mut buf = Vec::new();
        f.take(len).read_to_end(&mut buf)?;
        Ok(buf)
    })
    .await?;
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
                busy: false,
            },
        )
    });
    Ok(json!({"id": id}))
}

async fn write(dev: &Device, p: &Value) -> ApiResult {
    let id = s(p, "id").unwrap_or("").to_string();
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
    let n = data.len() as u64;
    let path = with_entries(|m| {
        let e = m
            .get_mut(&id)
            .filter(|e| e.owner == dev.id)
            .ok_or_else(|| err("not_found", "no such handoff"))?;
        let Dir::In { expected_size, .. } = &e.dir else {
            return Err(err("not_found", "no such handoff"));
        };
        if e.busy {
            return Err(err("conflict", "another write is in progress"));
        }
        if offset != e.size {
            return Err(err("conflict", format!("expected offset {}", e.size)));
        }
        if e.size + n > *expected_size {
            return Err(err("too_large", "more data than announced"));
        }
        e.busy = true;
        Ok(e.path.clone())
    })?;
    // Truncate to the offset first, so a failed write can simply be retried.
    let wrote = blocking(move || {
        let mut f = std::fs::OpenOptions::new().write(true).open(&path)?;
        f.set_len(offset)?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(&data)
    })
    .await;
    with_entries(|m| {
        let e = m
            .get_mut(&id)
            .ok_or_else(|| err("not_found", "no such handoff"))?;
        e.busy = false;
        wrote?;
        e.size += n;
        Ok(json!({"received": e.size}))
    })
}

// ---------------------------------------------------------------------------------------------
// import

async fn finish(gw: &Arc<Gateway>, dev: &Device, p: &Value) -> ApiResult {
    let id = s(p, "id").unwrap_or("").to_string();
    let (path, manifest, sha) = with_entries(|m| {
        let e = m
            .get_mut(&id)
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
        if e.busy {
            return Err(err("conflict", "this handoff is busy"));
        }
        if e.size != *expected_size {
            return Err(err(
                "conflict",
                format!("received {} of {} bytes", e.size, expected_size),
            ));
        }
        let r = (e.path.clone(), manifest.clone(), sha256.clone());
        e.busy = true;
        Ok(r)
    })?;
    let r = deliver(gw, dev, p, &path, &manifest, &sha).await;
    match &r {
        Ok(_) => remove_entry(&id, &dev.id),
        Err(_) => with_entries(|m| {
            if let Some(e) = m.get_mut(&id) {
                e.busy = false;
            }
        }),
    }
    r
}

/// Hand the verified bundle to the server as an incoming handoff (spec 16 §15.2). The server keeps
/// its own copy and decides: a pending record the receiver accepts later, or an automatic import.
/// A full device may still place the work right away (`repo_path`, `worktree_path`, `branch`),
/// which accepts the record on its behalf.
async fn deliver(
    gw: &Arc<Gateway>,
    dev: &Device,
    p: &Value,
    path: &Path,
    manifest: &Manifest,
    sha: &str,
) -> ApiResult {
    let p2 = path.to_path_buf();
    let (_, got) = blocking(move || hash_file(&p2)).await?;
    if got != sha {
        return Err(err("conflict", "checksum mismatch; send the handoff again"));
    }

    // A teammate's invitation never chooses where work lands.
    let teammate = dev.kind == "handoff";
    for k in ["repo_path", "worktree_path", "branch"] {
        if teammate && s(p, k).is_some() {
            return Err(err(
                "forbidden",
                "a handoff invitation cannot choose where work lands",
            ));
        }
    }

    let actor = format!("gateway:{}", dev.name);
    still_authorized(gw, dev)?;
    // The authenticated device that carried it, so the server's per-sender quota and job
    // identity never rest on the manifest's host text alone.
    let from = json!({"host": clean(&manifest.source_host, 100),
                      "owner": if teammate { "teammate" } else { "self" },
                      "device": dev.id});
    let mut rec =
        crate::handoff_peer::deliver_to_server(gw, &actor, path, manifest, sha, &from).await?;
    let id = s(&rec, "id").unwrap_or_default().to_string();

    if let Some(repo) = s(p, "repo_path").filter(|_| !teammate) {
        let mut params = json!({"id": id, "repo": {"path": expand_home(repo)},
                                "start_agent": p.get("start_agent").and_then(|v| v.as_bool()).unwrap_or(true)});
        if let Some(w) = s(p, "worktree_path") {
            params["worktree_path"] = json!(expand_home(w));
        }
        if let Some(b) = s(p, "branch") {
            params["branch"] = json!(b);
        }
        still_authorized(gw, dev)?;
        let accepted = gw.server.call_as(&actor, "handoff.accept", params).await?;
        rec = accepted.get("incoming").cloned().unwrap_or(rec);
    }
    gw.state.audit(&json!({"ts": now_s(), "event": "handoff.delivered", "device": dev.id, "from": manifest.source_host, "incoming": id, "state": s(&rec, "state")}));
    let mut out = crate::handoff_peer::outcome(&rec);
    out["record"] = rec;
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// incoming handoffs on this host (full-scope devices)

/// The server's incoming-handoff methods, with only the params they take.
async fn incoming(gw: &Arc<Gateway>, dev: &Device, method: &str, p: &Value) -> ApiResult {
    let keys: &[&str] = match method {
        "handoff.incoming.list" => &[],
        "handoff.incoming.get" | "handoff.decline" | "handoff.resume" => &["id"],
        // Read, or set whether the user's own handoffs may import without asking.
        "handoff.prefs" => &["always_ask"],
        "handoff.accept" => &[
            "id",
            "repo",
            "worktree_path",
            "branch",
            "start_agent",
            "trust",
        ],
        _ => return Err(err("method_not_found", method)),
    };
    let mut params = json!({});
    for k in keys {
        if let Some(v) = p.get(*k).filter(|v| !v.is_null()) {
            params[*k] = v.clone();
        }
    }
    if matches!(method, "handoff.incoming.list" | "handoff.incoming.get") {
        return gw.server.call(method, params).await;
    }
    still_authorized(gw, dev)?;
    gw.server
        .call_as(&format!("gateway:{}", dev.name), method, params)
        .await
}
