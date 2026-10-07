//! Incoming handoffs (spec 16 §15.2): work another host handed to this one waits here until the
//! receiver accepts it (or an automatic import does), then lands as a new worktree, a workspace
//! and, when the harness is known, a resumed agent.
//!
//! The gateway receives the bundle and hands it over with `handoff.incoming.add`; the bundle is
//! kept under `<state>/handoffs/in/<id>.tar.zst` (0700 directory) and the record in the kv store
//! (`handoff.incoming`), so every client sees the same list through `handoff.incoming.list|get`
//! and the `handoff.incoming` / `handoff.updated` events. `handoff.accept` runs the transactional
//! import of `vk-handoff`; a failed import keeps the bundle so the receiver can try again.
//! Records expire after 7 days; a sweep at start and every hour removes expired records, the
//! bundles of imported or declined ones and files no record refers to.
//!
//! Remembered placement lives in the kv scope `prefs`: `handoff.placement` (origin → repository
//! and worktree parent), `handoff.repos` (repositories handoffs were imported into) and
//! `handoff.always_ask`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vk_handoff::{Manifest, clean, git_line, hash_file, known_harness, remote_parts, same_remote};
use vk_proto::rpc::{ErrorKind, RpcError};
use vk_store::now_ms;

use crate::Server;
use crate::api::{Ctx, R, err, internal, invalid, not_found, req, s};
use crate::core::Tx;

pub const METHODS: &[(&str, bool)] = &[
    ("handoff.incoming.add", true),
    ("handoff.incoming.list", false),
    ("handoff.incoming.get", false),
    ("handoff.accept", true),
    ("handoff.decline", true),
    ("handoff.resume", true),
    ("handoff.prefs", true),
];

/// Where someone else's work lands and whether an agent starts on it is the user's decision.
pub const PANE_FORBIDDEN: &[&str] = &[
    "handoff.incoming.add",
    "handoff.incoming.list",
    "handoff.incoming.get",
    "handoff.accept",
    "handoff.decline",
    "handoff.resume",
    "handoff.prefs",
];

/// Shared definitions (see `api_schema::DEFS`).
pub const DEFS: &str = r##"
HandoffFrom = {host: string, owner: self|teammate, user?: string}
HandoffSummary = {source_host: string, repo_name: string, origin: string|null, branch: string|null, head: string, harness: string|null, session_id: string|null, cwd_rel: string, skipped: [{path: string, reason: string}], last_message: string|null, untracked: int, transcript: bool, redactions: int, created_at: int}
HandoffIncoming = {id: string, from: HandoffFrom, manifest: HandoffSummary, size: int, bundle_path: string|null, state: pending|importing|imported|failed|declined, error: {kind: string, message: string, details?: any}|null, result: object|null, created_at_ms: int, updated_at_ms: int, expires_at_ms: int}
"##;

pub const SHAPES: &str = r##"
# --- incoming handoffs (16 §15.2); full scope only ---
# the gateway hands over a received bundle (it may delete its copy afterwards); idempotent by sha256. Only gateway clients may call it
handoff.incoming.add :: {path: string, manifest: object, sha256: string, from: HandoffFrom, actor?: string}
  => {incoming: HandoffIncoming}
handoff.incoming.list :: {} => {incoming: [HandoffIncoming]}
# with where the work would land: matching clones, the remembered or default worktree path and branch
handoff.incoming.get :: {id: string}
  => {incoming: HandoffIncoming, suggested: {repos: [string], repo: string|null, worktree_path: string|null, branch: string}}
# import into a clone (any remote must match the origin, else conflict `repo_mismatch`) or a fresh clone; idempotent once imported
handoff.accept :: {id: string, repo: {path: string} | {clone_to: string}, worktree_path?: string, branch?: string, start_agent?: bool = true, trust?: [mise|direnv], actor?: string}
  => {incoming: HandoffIncoming}
handoff.decline :: {id: string, actor?: string} => {incoming: HandoffIncoming}
# start the agent again in the imported pane with the rebuilt resume arguments
handoff.resume :: {id: string, actor?: string} => {incoming: HandoffIncoming, run?: any, agent_error?: any}
# read, or with `always_ask` set, whether own handoffs may import without asking
handoff.prefs :: {always_ask?: bool, actor?: string}
  => {always_ask: bool, placement: {*: {repo: string, worktree_parent: string}}, repos: [string]}
"##;

pub const EVENTS: &str = r##"
handoff.incoming :: {incoming: string} => {incoming: HandoffIncoming}
handoff.updated :: {incoming: string} => {incoming: HandoffIncoming, phase: string|null}
handoff.expired :: {incoming: string} => {}
"##;

const SCOPE: &str = "handoff.incoming";
const PREFS: &str = "prefs";
const TTL_MS: i64 = 7 * 24 * 3600 * 1000;
const SWEEP_EVERY: Duration = Duration::from_secs(3600);
const MAX_REMEMBERED_REPOS: usize = 50;

/// Ids whose bundle file exists before (or while) its record says so: the sweep leaves them.
static BUSY: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    pub host: String,
    /// `self` (one of the user's own hosts) or `teammate` (a handoff invitation).
    pub owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incoming {
    pub id: String,
    pub from: Sender,
    pub manifest: Manifest,
    pub sha256: String,
    pub size: u64,
    /// Empty once the bundle is gone (imported, declined).
    pub bundle_path: String,
    /// `pending` | `importing` | `imported` | `failed` | `declined`.
    pub state: String,
    pub error: Option<Value>,
    pub result: Option<Value>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub repo: PathBuf,
    pub worktree_parent: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct Prefs {
    pub always_ask: bool,
    /// By [`origin_key`].
    pub placement: BTreeMap<String, Placement>,
    pub repos: Vec<PathBuf>,
}

/// Where an accepted handoff goes.
#[derive(Debug, Clone)]
enum RepoChoice {
    Path(PathBuf),
    CloneTo(PathBuf),
}

#[derive(Debug, Clone)]
struct Choice {
    repo: RepoChoice,
    worktree: Option<PathBuf>,
    branch: Option<String>,
    start_agent: bool,
    trust: Vec<String>,
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if !method.starts_with("handoff.") {
        return None;
    }
    Some(match method {
        "handoff.incoming.add" => add(server, ctx, p).await,
        "handoff.incoming.list" => {
            let now = now_ms();
            let list: Vec<Value> = all(server)
                .iter()
                .filter(|r| r.expires_at_ms > now)
                .map(view)
                .collect();
            Ok(json!({"incoming": list}))
        }
        "handoff.incoming.get" => get(server, p).await,
        "handoff.accept" => accept(server, p).await,
        "handoff.decline" => decline(server, p),
        "handoff.resume" => resume(server, p).await,
        "handoff.prefs" => prefs_api(server, p),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// records

fn bump(e: vk_handoff::Error) -> RpcError {
    let kind = match e.kind {
        "conflict" => ErrorKind::Conflict,
        "timeout" => ErrorKind::Timeout,
        "invalid_params" => ErrorKind::InvalidParams,
        "not_found" => ErrorKind::NotFound,
        "unsupported" => ErrorKind::Unsupported,
        _ => ErrorKind::Internal,
    };
    err(kind, e.message)
}

fn error_json(e: &RpcError) -> Value {
    let mut v = json!({"kind": e.data.kind, "message": e.message});
    if !e.data.details.is_null() {
        v["details"] = e.data.details.clone();
    }
    v
}

fn parse(s: &str) -> Option<Incoming> {
    serde_json::from_str(s).ok()
}

pub(crate) fn load(server: &Server, id: &str) -> Option<Incoming> {
    server.with_core(|c| {
        c.store
            .kv_get(SCOPE, id)
            .ok()
            .flatten()
            .and_then(|s| parse(&s))
    })
}

/// Every record, newest first.
pub(crate) fn all(server: &Server) -> Vec<Incoming> {
    let mut v: Vec<Incoming> = server.with_core(|c| {
        c.store
            .kv_scope(SCOPE)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, s)| parse(&s))
            .collect()
    });
    v.sort_by(|a, b| b.created_at_ms.cmp(&a.created_at_ms).then(b.id.cmp(&a.id)));
    v
}

/// Persist a new record and announce it (`handoff.incoming`).
pub(crate) fn save(server: &Server, r: &Incoming) -> Result<(), RpcError> {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.kv(SCOPE, &r.id, serde_json::to_string(r).ok());
    tx.event(
        "handoff.incoming",
        json!({"incoming": r.id}),
        json!({"incoming": view(r)}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(())
}

/// Read, change and write one record under the core lock; emits `handoff.updated`.
fn update(
    server: &Server,
    id: &str,
    phase: Option<&str>,
    f: impl FnOnce(&mut Incoming) -> Result<(), RpcError>,
) -> Result<Incoming, RpcError> {
    let mut c = server.core.lock().unwrap();
    let mut r = c
        .store
        .kv_get(SCOPE, id)
        .ok()
        .flatten()
        .and_then(|s| parse(&s))
        .ok_or_else(|| not_found("handoff", id))?;
    f(&mut r)?;
    r.updated_at_ms = now_ms();
    let mut tx = Tx::new();
    tx.m.kv(SCOPE, &r.id, serde_json::to_string(&r).ok());
    tx.event(
        "handoff.updated",
        json!({"incoming": r.id}),
        json!({"incoming": view(&r), "phase": phase}),
    );
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(r)
}

/// The API form of a record: a summary of the manifest, sender-controlled text cleaned.
pub fn view(r: &Incoming) -> Value {
    let m = &r.manifest;
    let mut from = json!({"host": clean(&r.from.host, 100), "owner": r.from.owner});
    if let Some(u) = &r.from.user {
        from["user"] = json!(clean(u, 200));
    }
    json!({
        "id": r.id,
        "from": from,
        "manifest": {
            "source_host": clean(&m.source_host, 100),
            "repo_name": clean(&m.repo_name, 200),
            "origin": m.origin.as_deref().map(|o| clean(o, 500)),
            "branch": m.branch.as_deref().map(|b| clean(b, 200)),
            "head": m.head,
            "harness": m.harness,
            "session_id": m.session_id,
            "cwd_rel": clean(&m.cwd_rel, 500),
            "skipped": m.skipped.iter().take(200).map(|x| json!({"path": clean(&x.path, 500), "reason": clean(&x.reason, 100)})).collect::<Vec<_>>(),
            "last_message": m.last_message.as_deref().map(|t| clean(t, 500)),
            "untracked": m.untracked.len(),
            "transcript": m.transcript_rel.is_some(),
            "redactions": m.redactions,
            "created_at": m.created_at,
        },
        "size": r.size,
        "bundle_path": (!r.bundle_path.is_empty()).then_some(&r.bundle_path),
        "state": r.state,
        "error": r.error,
        "result": r.result,
        "created_at_ms": r.created_at_ms,
        "updated_at_ms": r.updated_at_ms,
        "expires_at_ms": r.expires_at_ms,
    })
}

fn in_dir(server: &Server) -> std::io::Result<PathBuf> {
    let base = server.paths.state.join("handoffs");
    crate::paths::ensure_private_dir(&base)?;
    let d = base.join("in");
    crate::paths::ensure_private_dir(&d)?;
    Ok(d)
}

fn busy(id: &str, on: bool) {
    let mut b = BUSY.lock().unwrap();
    if on {
        b.insert(id.to_string());
    } else {
        b.remove(id);
    }
}

// ---------------------------------------------------------------------------------------------
// prefs

/// `git@github.com:a/b.git` and `https://github.com/a/b` remember the same placement.
pub fn origin_key(origin: &str) -> String {
    match remote_parts(origin) {
        Some((h, p)) if h.is_empty() => p,
        Some((h, p)) => format!("{h}/{p}"),
        None => origin.trim().to_string(),
    }
}

pub fn prefs(server: &Server) -> Prefs {
    server.with_core(|c| {
        let get = |k: &str| c.store.kv_get(PREFS, k).ok().flatten();
        Prefs {
            always_ask: get("handoff.always_ask").is_some_and(|v| v == "true"),
            placement: get("handoff.placement")
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or_default(),
            repos: get("handoff.repos")
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or_default(),
        }
    })
}

/// Remember where work from `origin` went: the repository and the worktree's parent.
fn remember(server: &Server, origin: Option<&str>, repo: &Path, worktree: &Path) {
    let mut p = prefs(server);
    if let Some(o) = origin {
        p.placement.insert(
            origin_key(o),
            Placement {
                repo: repo.to_path_buf(),
                worktree_parent: worktree
                    .parent()
                    .unwrap_or_else(|| repo.parent().unwrap_or(repo))
                    .to_path_buf(),
            },
        );
    }
    p.repos.retain(|r| r != repo);
    p.repos.insert(0, repo.to_path_buf());
    p.repos.truncate(MAX_REMEMBERED_REPOS);
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.kv(
        PREFS,
        "handoff.placement",
        serde_json::to_string(&p.placement).ok(),
    );
    tx.m.kv(PREFS, "handoff.repos", serde_json::to_string(&p.repos).ok());
    let _ = server.commit(&mut c, tx);
}

fn prefs_json(p: &Prefs) -> Value {
    json!({"always_ask": p.always_ask, "placement": p.placement, "repos": p.repos})
}

fn prefs_api(server: &Server, p: &Value) -> R {
    if let Some(v) = p.get("always_ask") {
        let on = v
            .as_bool()
            .ok_or_else(|| invalid("always_ask must be a boolean"))?;
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.kv(PREFS, "handoff.always_ask", Some(on.to_string()));
        server.commit(&mut c, tx).map_err(internal)?;
    }
    Ok(prefs_json(&prefs(server)))
}

// ---------------------------------------------------------------------------------------------
// repositories

/// The main checkout of the repository `dir` belongs to (a linked worktree's main repository).
pub async fn repo_root(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let top = git_line(dir, &["rev-parse", "--show-toplevel"]).await?;
    if let Some(common) = git_line(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await
    {
        let common = PathBuf::from(common);
        if common.file_name().is_some_and(|n| n == ".git")
            && let Some(main) = common.parent()
            && main.is_dir()
        {
            return Some(main.to_path_buf());
        }
    }
    Some(PathBuf::from(top))
}

/// Every remote URL of a repository.
pub async fn remotes(root: &Path) -> Vec<String> {
    let out = git_line(root, &["remote", "-v"]).await.unwrap_or_default();
    let mut v: Vec<String> = out
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Clones of `origin` this host knows: the repositories of open workspaces and the ones
/// handoffs were imported into before. Any remote counts, not just `origin`.
pub async fn matching_clones(server: &Server, origin: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = server.with_core(|c| {
        c.model
            .workspaces
            .iter()
            .map(|w| PathBuf::from(&w.root_path))
            .collect()
    });
    let p = prefs(server);
    dirs.extend(p.placement.values().map(|x| x.repo.clone()));
    dirs.extend(p.repos.iter().cloned());
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for d in dirs {
        let Some(root) = repo_root(&d).await else {
            continue;
        };
        if seen.contains(&root) {
            continue;
        }
        seen.push(root.clone());
        if remotes(&root).await.iter().any(|u| same_remote(u, origin)) {
            out.push(root);
        }
    }
    out
}

fn slug(branch: &str) -> String {
    branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// `<parent>/<repo>-handoff-<branch>`, with `-2`, `-3`, ... when taken.
fn free_worktree_path(parent: &Path, repo_name: &str, branch: &str) -> PathBuf {
    let base = format!("{repo_name}-handoff-{}", slug(branch));
    (1..100)
        .map(|n| {
            if n == 1 {
                parent.join(&base)
            } else {
                parent.join(format!("{base}-{n}"))
            }
        })
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .unwrap_or_else(|| parent.join(base))
}

fn dir_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".into())
}

/// Automatic import (16 §15.2): only the user's own handoffs, into a clone this host knows, at a
/// placement remembered for that origin, and only while `handoff.always_ask` is off. Returns the
/// repository and the worktree parent.
pub fn auto_placement(
    owner: &str,
    always_ask: bool,
    clones: &[PathBuf],
    remembered: Option<&Placement>,
) -> Option<(PathBuf, PathBuf)> {
    if owner != "self" || always_ask {
        return None;
    }
    let remembered = remembered?;
    let repo = clones
        .iter()
        .find(|c| **c == remembered.repo)
        .or(clones.first())?;
    Some((repo.clone(), remembered.worktree_parent.clone()))
}

async fn suggested(server: &Server, r: &Incoming) -> Value {
    let m = &r.manifest;
    let repos = match m.origin.as_deref() {
        Some(o) => matching_clones(server, o).await,
        None => Vec::new(),
    };
    let p = prefs(server);
    let remembered = m
        .origin
        .as_deref()
        .and_then(|o| p.placement.get(&origin_key(o)));
    let repo = remembered
        .map(|x| x.repo.clone())
        .filter(|x| repos.contains(x))
        .or_else(|| repos.first().cloned());
    let parent = remembered.map(|x| x.worktree_parent.clone()).or_else(|| {
        repo.as_ref()
            .and_then(|r| r.parent().map(Path::to_path_buf))
    });
    let base = m.branch.clone().unwrap_or_else(|| "detached".into());
    let name = repo
        .as_deref()
        .map(dir_name)
        .unwrap_or_else(|| slug(&clean(&m.repo_name, 100)));
    let worktree = parent.map(|p| free_worktree_path(&p, &name, &base));
    json!({"repos": repos, "repo": repo, "worktree_path": worktree, "branch": format!("handoff/{}", slug(&base))})
}

// ---------------------------------------------------------------------------------------------
// handoff.incoming.add

fn parse_from(p: &Value) -> Result<Sender, RpcError> {
    let f = p
        .get("from")
        .ok_or_else(|| invalid("missing param `from`"))?;
    let host = f
        .get("host")
        .and_then(Value::as_str)
        .map(|h| clean(h, 100))
        .filter(|h| !h.trim().is_empty())
        .ok_or_else(|| invalid("from.host is required"))?;
    let owner = match f.get("owner").and_then(Value::as_str) {
        Some(o @ ("self" | "teammate")) => o.to_string(),
        _ => return Err(invalid("from.owner must be self or teammate")),
    };
    let user = f
        .get("user")
        .and_then(Value::as_str)
        .map(|u| clean(u, 200))
        .filter(|u| !u.is_empty());
    Ok(Sender { host, owner, user })
}

/// Take the gateway's bundle: link (or copy) it in, check its checksum and that `manifest` is the
/// one inside. Runs off the async threads.
fn take_bundle(
    src: &Path,
    dest: &Path,
    dir: &Path,
    sha: &str,
    manifest: &Manifest,
) -> Result<u64, RpcError> {
    let md =
        std::fs::symlink_metadata(src).map_err(|e| invalid(format!("{}: {e}", src.display())))?;
    if !md.is_file() {
        return Err(invalid("path must be a regular file"));
    }
    if md.len() > vk_handoff::MAX_BUNDLE {
        return Err(invalid("handoff bundle exceeds 200 MiB"));
    }
    if std::fs::hard_link(src, dest).is_err() {
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)
            .map_err(internal)?;
        std::io::copy(&mut std::fs::File::open(src).map_err(internal)?, &mut out)
            .map_err(internal)?;
        out.sync_all().map_err(internal)?;
    }
    vk_store::restrict_file(dest);
    let (size, got) = hash_file(dest).map_err(internal)?;
    if got != sha {
        return Err(err(
            ErrorKind::Conflict,
            "checksum mismatch; send the handoff again",
        ));
    }
    let work = workdir(dir)?;
    let packed =
        vk_handoff::unpack(dest, work.path()).map_err(|e| invalid(format!("bad bundle: {e}")))?;
    vk_handoff::verify(&packed, manifest).map_err(bump)?;
    Ok(size)
}

fn workdir(dir: &Path) -> Result<tempfile::TempDir, RpcError> {
    tempfile::Builder::new()
        .prefix(".work-")
        .tempdir_in(dir)
        .map_err(internal)
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, RpcError> + Send + 'static,
) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(f).await.map_err(internal)?
}

async fn add(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    if ctx.kind != "gateway" || ctx.pane_scope.is_some() {
        return Err(err(
            ErrorKind::PermissionDenied,
            "handoff.incoming.add is called by the gateway that received the handoff",
        ));
    }
    let path = PathBuf::from(req(p, "path")?);
    if !path.is_absolute() {
        return Err(invalid("path must be absolute"));
    }
    let manifest: Manifest = serde_json::from_value(p.get("manifest").cloned().unwrap_or_default())
        .map_err(|e| invalid(format!("manifest: {e}")))?;
    let sha = req(p, "sha256")?.to_ascii_lowercase();
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("sha256 must be 64 hex digits"));
    }
    let from = parse_from(p)?;

    // Idempotent: a gateway that lost the answer hands the same bundle over again.
    if let Some(r) = all(server)
        .into_iter()
        .find(|r| r.sha256 == sha && r.state != "declined")
    {
        return Ok(json!({"incoming": view(&r)}));
    }

    let id = ulid::Ulid::new().to_string().to_lowercase();
    let dir = in_dir(server).map_err(internal)?;
    let dest = dir.join(format!("{id}.tar.zst"));
    busy(&id, true);
    let taken = {
        let (src, dest, dir, sha, m) = (
            path.clone(),
            dest.clone(),
            dir.clone(),
            sha.clone(),
            manifest.clone(),
        );
        blocking(move || take_bundle(&src, &dest, &dir, &sha, &m)).await
    };
    let size = match taken {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(&dest);
            busy(&id, false);
            return Err(e);
        }
    };
    let now = now_ms();
    let rec = Incoming {
        id: id.clone(),
        from,
        manifest,
        sha256: sha,
        size,
        bundle_path: dest.display().to_string(),
        state: "pending".into(),
        error: None,
        result: None,
        created_at_ms: now,
        updated_at_ms: now,
        expires_at_ms: now + TTL_MS,
    };
    let saved = save(server, &rec);
    busy(&id, false);
    if let Err(e) = saved {
        let _ = std::fs::remove_file(&dest);
        return Err(e);
    }
    // The user's own host, a known clone and a remembered placement: import right away.
    let m = &rec.manifest;
    let pr = prefs(server);
    let auto = match m.origin.as_deref() {
        Some(o) if rec.from.owner == "self" && !pr.always_ask => {
            let clones = matching_clones(server, o).await;
            auto_placement(
                &rec.from.owner,
                pr.always_ask,
                &clones,
                pr.placement.get(&origin_key(o)),
            )
        }
        _ => None,
    };
    let branch = clean(m.branch.as_deref().unwrap_or("detached"), 100);
    let Some((repo, parent)) = auto else {
        server.notify(
            "handoff",
            None,
            &format!("Incoming handoff from {}: {branch}", rec.from.host),
            &format!(
                "{} is waiting to be accepted or declined (handoff {id}).",
                clean(&m.repo_name, 100)
            ),
            "normal",
        );
        return Ok(json!({"incoming": view(&rec)}));
    };
    let choice = Choice {
        worktree: Some(free_worktree_path(
            &parent,
            &dir_name(&repo),
            m.branch.as_deref().unwrap_or("detached"),
        )),
        repo: RepoChoice::Path(repo),
        branch: None,
        start_agent: true,
        trust: Vec::new(),
    };
    let rec = match claim(server, &id)? {
        Claimed::Run(r) => r,
        Claimed::Done(r) => return Ok(json!({"incoming": view(&r)})),
    };
    let (srv, r2) = (server.clone(), rec.clone());
    tokio::spawn(async move {
        let host = r2.from.host.clone();
        match finish_accept(&srv, &r2, &choice).await {
            Ok(done) => {
                let br = done
                    .result
                    .as_ref()
                    .and_then(|v| v.get("branch"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let pane = done
                    .result
                    .as_ref()
                    .and_then(|v| v.get("pane"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                srv.notify(
                    "handoff",
                    pane.as_deref(),
                    &format!("Handoff from {host} is ready: {br}"),
                    "Imported automatically at the remembered place.",
                    "normal",
                );
            }
            Err(e) => {
                srv.notify(
                    "handoff",
                    None,
                    &format!("Handoff from {host} could not be imported"),
                    &format!(
                        "{} (handoff {}). Accept it to choose another place.",
                        e.message, r2.id
                    ),
                    "normal",
                );
            }
        }
    });
    Ok(json!({"incoming": view(&rec)}))
}

// ---------------------------------------------------------------------------------------------
// handoff.incoming.get / accept / decline / resume

async fn get(server: &Server, p: &Value) -> R {
    let id = req(p, "id")?;
    let r = load(server, id).ok_or_else(|| not_found("handoff", id))?;
    let suggested = suggested(server, &r).await;
    Ok(json!({"incoming": view(&r), "suggested": suggested}))
}

fn abs(p: &Value, what: &str) -> Result<PathBuf, RpcError> {
    let raw = p
        .as_str()
        .filter(|x| !x.is_empty())
        .ok_or_else(|| invalid(format!("{what} must be a path")))?;
    let path = vk_handoff::expand_home(raw);
    if !path.is_absolute() {
        return Err(invalid(format!("{what} must be an absolute path")));
    }
    Ok(path)
}

fn parse_choice(p: &Value) -> Result<Choice, RpcError> {
    let repo = p
        .get("repo")
        .ok_or_else(|| invalid("missing param `repo` ({path} or {clone_to})"))?;
    let repo = match (repo.get("path"), repo.get("clone_to")) {
        (Some(x), None) => RepoChoice::Path(abs(x, "repo.path")?),
        (None, Some(x)) => RepoChoice::CloneTo(abs(x, "repo.clone_to")?),
        _ => return Err(invalid("repo is {path} or {clone_to}")),
    };
    let worktree = match p.get("worktree_path") {
        None | Some(Value::Null) => None,
        Some(x) => Some(abs(x, "worktree_path")?),
    };
    let branch = s(p, "branch").filter(|b| !b.is_empty()).map(str::to_string);
    let start_agent = p
        .get("start_agent")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let mut trust = Vec::new();
    for t in p
        .get("trust")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match t.as_str() {
            Some(x @ ("mise" | "direnv")) => {
                if !trust.iter().any(|y| y == x) {
                    trust.push(x.to_string());
                }
            }
            _ => return Err(invalid("trust takes mise and direnv")),
        }
    }
    Ok(Choice {
        repo,
        worktree,
        branch,
        start_agent,
        trust,
    })
}

enum Claimed {
    Run(Incoming),
    Done(Incoming),
}

/// Move a record to `importing`, or say it is already imported.
fn claim(server: &Server, id: &str) -> Result<Claimed, RpcError> {
    if let Some(r) = load(server, id).filter(|r| r.state == "imported") {
        return Ok(Claimed::Done(r));
    }
    update(server, id, Some("importing"), |r| {
        match r.state.as_str() {
            "importing" => {
                return Err(err(ErrorKind::Conflict, "this handoff is being imported"));
            }
            "imported" => {
                return Err(err(ErrorKind::Conflict, "this handoff was just imported"));
            }
            "declined" => return Err(err(ErrorKind::Conflict, "this handoff was declined")),
            _ => {}
        }
        if r.expires_at_ms <= now_ms() {
            return Err(err(ErrorKind::NotFound, "this handoff expired"));
        }
        if r.bundle_path.is_empty() {
            return Err(err(ErrorKind::Conflict, "the handoff's bundle is gone"));
        }
        r.state = "importing".into();
        r.error = None;
        Ok(())
    })
    .map(Claimed::Run)
}

async fn accept(server: &Arc<Server>, p: &Value) -> R {
    let id = req(p, "id")?;
    let choice = parse_choice(p)?;
    let r = match claim(server, id)? {
        Claimed::Done(r) => return Ok(json!({"incoming": view(&r)})),
        Claimed::Run(r) => r,
    };
    let done = finish_accept(server, &r, &choice).await?;
    Ok(json!({"incoming": view(&done)}))
}

/// Run the import for a claimed record and record its outcome: `imported` (bundle removed) or
/// `failed` with the error (bundle kept, so it can be accepted again).
async fn finish_accept(
    server: &Arc<Server>,
    r: &Incoming,
    choice: &Choice,
) -> Result<Incoming, RpcError> {
    busy(&r.id, true);
    let out = run_accept(server, r, choice).await;
    busy(&r.id, false);
    match out {
        Ok(result) => {
            let bundle = r.bundle_path.clone();
            let done = update(server, &r.id, None, |x| {
                x.state = "imported".into();
                x.error = None;
                x.result = Some(result);
                x.bundle_path.clear();
                Ok(())
            })?;
            if !bundle.is_empty() {
                let _ = std::fs::remove_file(&bundle);
            }
            Ok(done)
        }
        Err(e) => {
            let ej = error_json(&e);
            let _ = update(server, &r.id, None, |x| {
                x.state = "failed".into();
                x.error = Some(ej);
                Ok(())
            });
            Err(e)
        }
    }
}

async fn check_origin(root: &Path, m: &Manifest) -> Result<(), RpcError> {
    let Some(origin) = m.origin.as_deref() else {
        return Ok(());
    };
    let remotes = remotes(root).await;
    if remotes.iter().any(|u| same_remote(u, origin)) {
        return Ok(());
    }
    Err(err(
        ErrorKind::Conflict,
        format!(
            "repo_mismatch: no remote of {} is {}",
            root.display(),
            clean(origin, 300)
        ),
    )
    .details(
        json!({"reason": "repo_mismatch", "repo": root, "origin": origin, "remotes": remotes}),
    ))
}

/// `git clone <origin> <to>` with the receiver's own credentials. `to` must not exist or be an
/// empty directory; its parent must exist.
async fn clone_into(m: &Manifest, to: &Path) -> Result<PathBuf, RpcError> {
    let origin = m
        .origin
        .as_deref()
        .ok_or_else(|| invalid("the handoff names no origin to clone"))?;
    if origin.starts_with('-') || origin.contains("::") || remote_parts(origin).is_none() {
        return Err(invalid(format!("cannot clone {}", clean(origin, 300))));
    }
    match std::fs::symlink_metadata(to) {
        Ok(md) => {
            let empty = md.is_dir()
                && std::fs::read_dir(to)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(false);
            if !empty {
                return Err(err(
                    ErrorKind::Conflict,
                    format!("{} exists and is not an empty directory", to.display()),
                ));
            }
        }
        Err(_) => {
            if !to.parent().is_some_and(Path::is_dir) {
                return Err(invalid(format!(
                    "the parent of {} does not exist",
                    to.display()
                )));
            }
        }
    }
    let parent = to.parent().unwrap_or(Path::new("/"));
    let dest = to
        .to_str()
        .ok_or_else(|| invalid("clone_to is not valid UTF-8"))?;
    vk_handoff::git(parent, &["clone", "--", origin, dest])
        .await
        .map_err(bump)?;
    repo_root(to).await.ok_or_else(|| {
        internal(format!(
            "{} is not a repository after cloning",
            to.display()
        ))
    })
}

/// The prompt an imported agent starts with (same text the gateway used).
fn note(m: &Manifest, cwd: &str, branch: &str) -> String {
    format!(
        "This session was handed off from {} ({}). The work continues in {} on branch {}. Files not carried over: {}.",
        clean(&m.source_host, 60),
        clean(&m.source_cwd, 200),
        cwd,
        branch,
        if m.skipped.is_empty() {
            "none".to_string()
        } else {
            m.skipped
                .iter()
                .take(20)
                .map(|s| clean(&s.path, 120))
                .collect::<Vec<_>>()
                .join(", ")
        }
    )
}

async fn start_agent(
    server: &Arc<Server>,
    pane: &str,
    harness: &str,
    prompt: &str,
    args: &[String],
) -> Result<Value, RpcError> {
    let opts = crate::sandbox::LaunchOpts::from_params(&json!({}))?;
    let r = crate::agents::start_in_pane_opts(
        server,
        pane,
        harness,
        None,
        Some(prompt),
        args,
        None,
        &opts,
    )
    .await?;
    Ok(r.get("run").cloned().unwrap_or(r))
}

async fn run_accept(server: &Arc<Server>, r: &Incoming, c: &Choice) -> Result<Value, RpcError> {
    let m = &r.manifest;
    let (root, cloned) = match &c.repo {
        RepoChoice::Path(p) => {
            let root = repo_root(p).await.ok_or_else(|| {
                err(
                    ErrorKind::NotFound,
                    format!("{} is not a git repository", p.display()),
                )
            })?;
            check_origin(&root, m).await?;
            (root, false)
        }
        RepoChoice::CloneTo(to) => {
            let _ = save_phase(server, r, "cloning");
            (clone_into(m, to).await?, true)
        }
    };
    let _ = save_phase(server, r, "importing");

    // The bundle is still the one that arrived, and its manifest the one recorded.
    let dir = in_dir(server).map_err(internal)?;
    let (bundle, sha, manifest) = (PathBuf::from(&r.bundle_path), r.sha256.clone(), m.clone());
    let (work, packed) = blocking(move || {
        let (_, got) = hash_file(&bundle).map_err(internal)?;
        if got != sha {
            return Err(err(
                ErrorKind::Conflict,
                "the stored bundle changed; send the handoff again",
            ));
        }
        let work = workdir(&dir)?;
        let packed = vk_handoff::unpack(&bundle, work.path())
            .map_err(|e| invalid(format!("bad bundle: {e}")))?;
        vk_handoff::verify(&packed, &manifest).map_err(bump)?;
        Ok((work, packed))
    })
    .await?;
    let imported = vk_handoff::import(
        work.path(),
        &packed,
        &root,
        c.worktree.as_deref(),
        c.branch.as_deref(),
    )
    .await
    .map_err(bump)?;
    drop(work);
    remember(server, m.origin.as_deref(), &root, &imported.worktree);
    let trust = trust(&imported.worktree, &imported.cwd, &c.trust).await;

    let cwd = imported.cwd.display().to_string();
    let mut result = json!({
        "repo": root,
        "cloned": cloned,
        "worktree": imported.worktree,
        "branch": imported.branch,
        "cwd": cwd,
        "resumed": imported.resumed,
        "resume_args": imported.resume_args,
        "not_written": imported.not_written,
        "skipped": m.skipped,
        "trust": trust,
        "workspace": null,
        "pane": null,
    });
    let _ = save_phase(server, r, "starting");
    match server.create_workspace(&cwd, None, None, None) {
        Ok((ws, _tab, pane)) => {
            result["workspace"] = json!(ws.id);
            result["pane"] = json!(pane.id);
            let harness = m.harness.as_deref().filter(|h| known_harness(h));
            if let (true, Some(h)) = (c.start_agent, harness) {
                // Rebuilt from the harness and a validated session id; never the manifest's argv.
                let args = imported
                    .resume_args
                    .clone()
                    .filter(|_| imported.resumed)
                    .unwrap_or_default();
                let prompt = note(m, &cwd, &imported.branch);
                match start_agent(server, &pane.id, h, &prompt, &args).await {
                    Ok(run) => result["run"] = run,
                    Err(e) => result["agent_error"] = serde_json::to_value(&e).unwrap_or_default(),
                }
            }
        }
        Err(e) => result["workspace_error"] = json!(e.to_string()),
    }
    Ok(result)
}

/// Announce progress without changing the state.
fn save_phase(server: &Server, r: &Incoming, phase: &str) -> Result<Incoming, RpcError> {
    update(server, &r.id, Some(phase), |_| Ok(()))
}

fn decline(server: &Server, p: &Value) -> R {
    let id = req(p, "id")?;
    let mut bundle = String::new();
    let r = update(server, id, None, |r| match r.state.as_str() {
        "declined" => Ok(()),
        "pending" | "failed" => {
            r.state = "declined".into();
            bundle = std::mem::take(&mut r.bundle_path);
            Ok(())
        }
        "importing" => Err(err(ErrorKind::Conflict, "this handoff is being imported")),
        _ => Err(err(
            ErrorKind::Conflict,
            "this handoff was already imported",
        )),
    })?;
    if !bundle.is_empty() {
        let _ = std::fs::remove_file(&bundle);
    }
    Ok(json!({"incoming": view(&r)}))
}

async fn resume(server: &Arc<Server>, p: &Value) -> R {
    let id = req(p, "id")?;
    let r = load(server, id).ok_or_else(|| not_found("handoff", id))?;
    if r.state != "imported" {
        return Err(err(
            ErrorKind::Conflict,
            format!("this handoff is {}, not imported", r.state),
        ));
    }
    let res = r.result.clone().unwrap_or_default();
    let pane = res
        .get("pane")
        .and_then(Value::as_str)
        .ok_or_else(|| err(ErrorKind::Conflict, "the import has no pane to start in"))?;
    if server.with_core(|c| c.pane(pane).is_none()) {
        return Err(not_found("pane", pane));
    }
    let harness = r
        .manifest
        .harness
        .as_deref()
        .filter(|h| known_harness(h))
        .ok_or_else(|| err(ErrorKind::Conflict, "the handoff names no known harness"))?;
    let args: Vec<String> = if res.get("resumed").and_then(Value::as_bool) == Some(true) {
        res.get("resume_args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let cwd = res.get("cwd").and_then(Value::as_str).unwrap_or_default();
    let branch = res
        .get("branch")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let prompt = note(&r.manifest, cwd, branch);
    let out = start_agent(server, pane, harness, &prompt, &args).await;
    let (run, agent_error) = match &out {
        Ok(run) => (Some(run.clone()), None),
        Err(e) => (None, Some(serde_json::to_value(e).unwrap_or_default())),
    };
    let r = update(server, id, None, |x| {
        if let Some(obj) = x.result.as_mut().and_then(Value::as_object_mut) {
            obj.remove("run");
            obj.remove("agent_error");
            if let Some(v) = &run {
                obj.insert("run".into(), v.clone());
            }
            if let Some(v) = &agent_error {
                obj.insert("agent_error".into(), v.clone());
            }
        }
        Ok(())
    })?;
    let mut v = json!({"incoming": view(&r)});
    if let Some(run) = run {
        v["run"] = run;
    }
    if let Some(e) = agent_error {
        v["agent_error"] = e;
    }
    Ok(v)
}

// ---------------------------------------------------------------------------------------------
// trust

fn which(bin: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(bin)).find(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// `mise trust` / `direnv allow` in the new worktree, only for the tools the receiver ticked and
/// only where their files exist.
async fn trust(wt: &Path, cwd: &Path, tools: &[String]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut dirs = vec![wt.to_path_buf()];
    if cwd != wt {
        dirs.push(cwd.to_path_buf());
    }
    for tool in tools {
        let (files, bin, verb): (&[&str], &str, &str) = match tool.as_str() {
            "mise" => (
                &["mise.toml", ".mise.toml", ".tool-versions"][..],
                "mise",
                "trust",
            ),
            "direnv" => (&[".envrc"][..], "direnv", "allow"),
            _ => continue,
        };
        let found: Vec<&PathBuf> = dirs
            .iter()
            .filter(|d| files.iter().any(|f| d.join(f).is_file()))
            .collect();
        if found.is_empty() {
            out.push(json!({"tool": tool, "status": "not_needed"}));
            continue;
        }
        let Some(exe) = which(bin) else {
            out.push(json!({"tool": tool, "status": "not_installed"}));
            continue;
        };
        for d in found {
            let run = tokio::time::timeout(
                Duration::from_secs(30),
                tokio::process::Command::new(&exe)
                    .arg(verb)
                    .arg(d)
                    .current_dir(d)
                    .stdin(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .output(),
            )
            .await;
            let v = match run {
                Ok(Ok(o)) if o.status.success() => {
                    json!({"tool": tool, "dir": d, "status": "trusted"})
                }
                Ok(Ok(o)) => {
                    json!({"tool": tool, "dir": d, "status": "failed", "error": clean(String::from_utf8_lossy(&o.stderr).trim(), 300)})
                }
                Ok(Err(e)) => {
                    json!({"tool": tool, "dir": d, "status": "failed", "error": e.to_string()})
                }
                Err(_) => {
                    json!({"tool": tool, "dir": d, "status": "failed", "error": "timed out"})
                }
            };
            out.push(v);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// sweep

/// Start-up recovery and the hourly sweep.
pub fn start(server: &Arc<Server>) {
    // An import a restart interrupted failed; its bundle is kept, so it can be accepted again.
    for r in all(server).into_iter().filter(|r| r.state == "importing") {
        let _ = update(server, &r.id, None, |x| {
            x.state = "failed".into();
            x.error =
                Some(json!({"kind": "internal", "message": "interrupted by a server restart"}));
            Ok(())
        });
    }
    let srv = server.clone();
    tokio::spawn(async move {
        let mut startup = true;
        loop {
            let s2 = srv.clone();
            let _ = tokio::task::spawn_blocking(move || sweep(&s2, startup)).await;
            startup = false;
            tokio::time::sleep(SWEEP_EVERY).await;
        }
    });
}

/// Remove expired records with their bundles, the bundles of imported and declined records, and
/// files no record refers to (work directories only at start, when no import can be running).
/// Returns how many files and records were removed.
pub fn sweep(server: &Server, startup: bool) -> usize {
    let now = now_ms();
    let mut removed = 0;
    let mut live = BTreeSet::new();
    for r in all(server) {
        let bundle = (!r.bundle_path.is_empty()).then(|| PathBuf::from(&r.bundle_path));
        if r.expires_at_ms <= now && r.state != "importing" {
            if let Some(b) = &bundle {
                let _ = std::fs::remove_file(b);
            }
            let mut c = server.core.lock().unwrap();
            let mut tx = Tx::new();
            tx.m.kv(SCOPE, &r.id, None);
            tx.event("handoff.expired", json!({"incoming": r.id}), json!({}));
            let _ = server.commit(&mut c, tx);
            removed += 1;
        } else if matches!(r.state.as_str(), "imported" | "declined") && bundle.is_some() {
            if let Some(b) = &bundle {
                let _ = std::fs::remove_file(b);
            }
            let _ = update(server, &r.id, None, |x| {
                x.bundle_path.clear();
                Ok(())
            });
            removed += 1;
        } else if let Some(b) = bundle {
            live.insert(b);
        }
    }
    let Ok(dir) = in_dir(server) else {
        return removed;
    };
    let busy: Vec<String> = BUSY.lock().unwrap().iter().cloned().collect();
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if live.contains(&p) || busy.iter().any(|id| name.starts_with(id.as_str())) {
            continue;
        }
        let r = match e.file_type() {
            Ok(t) if t.is_dir() => {
                if !startup {
                    continue;
                }
                std::fs::remove_dir_all(&p)
            }
            _ => std::fs::remove_file(&p),
        };
        match r {
            Ok(()) => removed += 1,
            Err(x) => tracing::warn!("handoff sweep: {}: {x}", p.display()),
        }
    }
    removed
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod tests;
