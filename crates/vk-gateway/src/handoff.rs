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
use vk_handoff::Manifest;

pub use vk_handoff::claude_project_dir;

use crate::Gateway;
use crate::api::{ApiError, ApiResult, normalize};
use crate::state::Device;

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
///
/// Every git command runs against the commit read at the start (or, with `expect`, the commit
/// the user approved, which must be the one checked out), never a later `HEAD`; the working-tree
/// changes and untracked files are live by nature, so after packing HEAD and the branch are read
/// again and the export fails with `repo_moved` if they changed meanwhile.
#[allow(clippy::too_many_arguments)]
pub async fn export_bundle(
    gw: &Arc<Gateway>,
    actor: &str,
    auth: Option<&Device>,
    pane: &str,
    interrupt: bool,
    full: bool,
    source_job: Option<&str>,
    expect: Option<&Expected>,
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

    let pinned = vk_handoff::pin(&cwd).await?;
    // An approved send: the repository must still be where the user approved it.
    if let Some(e) = expect {
        e.check(
            &pinned.root.display().to_string(),
            pinned.branch.as_deref(),
            &pinned.head,
        )?;
    }

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
    let input = vk_handoff::ExportInput {
        cwd,
        pin: Some(pinned),
        harness: s(&run, "harness").map(str::to_string),
        session_id: s(&run, "harness_session_id").map(str::to_string),
        transcript: s(&run, "transcript_path").map(PathBuf::from),
        resume_args,
        last_message: s(&run, "last_message").map(str::to_string),
        source_host: gw.host_name.clone(),
        source_job: source_job.map(str::to_string),
        full,
    };
    let id = ulid::Ulid::new().to_string().to_lowercase();
    let out_path = dir(gw)?.join(format!("{id}.out.tar.zst"));
    let packed = vk_handoff::export(&input, &out_path).await?;
    Ok(Exported {
        id,
        path: out_path,
        size: packed.size,
        sha256: packed.sha256,
        manifest: packed.manifest,
    })
}

/// What the user approved for a send from a pane (`auth.approve`), recorded at request time
/// (the job's `expect`): the export must come from this repository, branch and commit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expected {
    pub repo_root: String,
    pub branch: Option<String>,
    pub head: String,
}

impl Expected {
    /// From a job's `expect` object; `None` when the job has none.
    pub fn from_json(v: &Value) -> Option<Self> {
        if !v.is_object() {
            return None;
        }
        Some(Self {
            repo_root: s(v, "repo_root").unwrap_or_default().to_string(),
            branch: s(v, "branch").map(str::to_string),
            head: s(v, "head").unwrap_or_default().to_string(),
        })
    }

    /// `repo_moved` unless the repository is at `root` on `branch` at `head`.
    pub fn check(&self, root: &str, branch: Option<&str>, head: &str) -> Result<(), ApiError> {
        if self.repo_root == root && self.branch.as_deref() == branch && self.head == head {
            return Ok(());
        }
        Err(err(
            "conflict",
            format!(
                "repo_moved: the pane's repository changed since the handoff was approved (approved: {}; now: {}); ask again",
                at(&self.repo_root, self.branch.as_deref(), &self.head),
                at(root, branch, head)
            ),
        ))
    }
}

fn at(root: &str, branch: Option<&str>, head: &str) -> String {
    let short: String = head.chars().take(12).collect();
    format!(
        "{root} on {} at {}",
        branch.unwrap_or("a detached HEAD"),
        if short.is_empty() {
            "no commit"
        } else {
            short.as_str()
        }
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expectation_matches_only_the_approved_repository_branch_and_commit() {
        let e = Expected::from_json(
            &json!({"repo_root": "/src/app", "branch": "main", "head": "a".repeat(40)}),
        )
        .unwrap();
        assert!(e.check("/src/app", Some("main"), &"a".repeat(40)).is_ok());
        for (root, branch, head) in [
            ("/src/other", Some("main"), "a".repeat(40)),
            ("/src/app", Some("feature"), "a".repeat(40)),
            ("/src/app", None, "a".repeat(40)),
            ("/src/app", Some("main"), "b".repeat(40)),
        ] {
            let x = e.check(root, branch, &head).unwrap_err();
            assert!(x.message.starts_with("repo_moved"), "{}", x.message);
        }
    }
}
