//! Handoff export and the incoming-handoff passthrough (spec 16 §15.2). The source side exports
//! an agent's work at a turn boundary as a bundle ([`export_bundle`]); `handoff_send.rs` carries
//! it to the peer host, whose `handoff_peer.rs` delivers it to its server as an incoming handoff,
//! which the receiver accepts (or the server imports automatically) as a new worktree with the
//! agent resumed there.
//!
//! Bundle format, unpacking and the repository-side import live in `vk-handoff`; the import and
//! the incoming records live in the server (`handoff.incoming.*`, `handoff.accept`). This module
//! does the export and forwards the receiver's own methods to the server.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use vk_handoff::{
    MAX_BUNDLE, MAX_UNTRACKED, Manifest, Skipped, git, git_line, hash_file, regular_under,
    safe_relative, secret_path,
};

pub use vk_handoff::claude_project_dir;

use crate::Gateway;
use crate::api::{ApiError, ApiResult, normalize};
use crate::state::{Device, now_s};

impl From<vk_handoff::Error> for ApiError {
    fn from(e: vk_handoff::Error) -> Self {
        ApiError::new(e.kind, e.message)
    }
}

/// Remove everything in the handoffs directory. Uploads live in memory, so at startup all that
/// is there is left over from before a restart.
pub fn sweep(gw: &Gateway) {
    let Ok(d) = dir(gw) else { return };
    for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
        let p = e.path();
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
