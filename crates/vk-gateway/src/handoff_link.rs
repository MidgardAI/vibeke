//! Continue links (spec 20 §4): a session as a link a teammate opens later, when this host may be
//! offline.
//!
//! - `handoff.link.create {pane, access: link|github, github?: [login], ttl_s?, interrupt?}` →
//!   `{job}` (`access: github` without `github` uses the account's sharing list). The job exports the pane's work at a turn boundary
//!   ([`crate::handoff::export_bundle`]), encrypts it with a fresh key
//!   ([`vk_handoff::stream`]), uploads the ciphertext to the account server with this host's
//!   account and builds the link ([`ContinueLink`]) and a Markdown snippet for a pull request.
//! - `handoff.link.redeem {link}` → `{job}`. The job reads the sender's identity from the account
//!   server's blob metadata, downloads the blob with this host's account, decrypts it, checks size
//!   and sha256 against the link, and hands the bundle to the server like a peer handoff
//!   ([`deliver_to_server`]) from a teammate, so it is never imported automatically.
//! - `handoff.link.status {job}` → `{job, kind, state, progress?, error?, error_kind?, link?,
//!   markdown?, blob?, expires_at?, incoming?, ...}`; `state` is `exporting`, `uploading`,
//!   `downloading`, `importing`, `done` or `failed`. `progress` is `{done, total}` in bytes.
//! - `handoff.link.list {}` → `{links: [{blob, job, pane, brief, created_at, expires_at, size,
//!   link, markdown}]}`: links this host created that are still valid.
//! - `handoff.link.revoke {blob}` deletes the blob on the account server.
//! - `handoff.cancel {id: <job>}` (and `handoff.link.cancel {job}`) stops a running job.
//!
//! Jobs are kept in `<state dir>/handoff-links.json` (0600; a created link holds its key). A job
//! that was running when the gateway stopped is `failed` with `interrupted` the next time any job
//! is read. Temporary files live in the handoffs directory, which the gateway sweeps at start.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;
use vk_account::Account;
use vk_account::blobs::{BlobAccess, Progress, error_code};
use vk_handoff::continue_link::{Brief, ContinueLink, valid_blob_id};
use vk_handoff::{Manifest, stream};

use crate::Gateway;
use crate::api::{ApiError, ApiResult, normalize};
use crate::handoff::{blocking, dir, export_bundle};
use crate::handoff_peer::deliver_to_server;
use crate::state::{Device, now_s};

/// Blob lifetime without `ttl_s`, and its bounds.
pub const DEFAULT_TTL_S: u64 = 7 * 24 * 3600;
pub const MIN_TTL_S: u64 = 300;
pub const MAX_TTL_S: u64 = 30 * 24 * 3600;
/// The account server's limit per blob.
pub const MAX_BLOB: u64 = 256 * 1024 * 1024;
/// Where links point when the gateway has no `app_url`.
pub const DEFAULT_APP_URL: &str = "https://app.vibeke.dev";
/// Job ids start with this, so `handoff.cancel` can tell them from the server's send jobs.
pub const JOB_PREFIX: &str = "hl-";
const FILE: &str = "handoff-links.json";
/// Finished redeem jobs and failed jobs are forgotten after this long.
const KEEP_S: u64 = 7 * 24 * 3600;

/// One create or redeem job, as stored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkJob {
    pub id: String,
    /// `create` | `redeem`
    pub kind: String,
    /// `exporting` | `uploading` | `downloading` | `importing` | `done` | `failed`
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    /// The continue link (create; it carries the key).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    /// Account server origin of the blob.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brief: Option<Brief>,
    /// Plaintext bundle size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Redeem: the server's incoming handoff id and state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incoming: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incoming_state: Option<String>,
    /// Redeem: the sender as the account server knows it (`{id, login}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender: Option<Value>,
    pub revoked: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

impl LinkJob {
    fn finished(&self) -> bool {
        matches!(self.state.as_str(), "done" | "failed")
    }
}

// ---------------------------------------------------------------------------------------------
// job store

/// Serializes reads and writes of the job file in this process.
static LOCK: Mutex<()> = Mutex::new(());
/// Jobs this process runs, with their cancel signals.
static RUNNING: Mutex<Option<HashMap<String, Arc<Notify>>>> = Mutex::new(None);
/// Bytes moved per running job: `(done, total)`.
static PROGRESS: Mutex<Option<HashMap<String, (u64, u64)>>> = Mutex::new(None);

fn running<T>(f: impl FnOnce(&mut HashMap<String, Arc<Notify>>) -> T) -> T {
    f(RUNNING.lock().unwrap().get_or_insert_with(HashMap::new))
}

fn progress<T>(f: impl FnOnce(&mut HashMap<String, (u64, u64)>) -> T) -> T {
    f(PROGRESS.lock().unwrap().get_or_insert_with(HashMap::new))
}

fn err(kind: &str, m: impl Into<String>) -> ApiError {
    ApiError::new(kind, m)
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    err("internal", e.to_string())
}

fn s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(|v| v.as_str())
}

fn req<'a>(p: &'a Value, k: &str) -> Result<&'a str, ApiError> {
    s(p, k)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::invalid(format!("{k} is required")))
}

fn path(gw: &Gateway) -> PathBuf {
    gw.state.dir.join(FILE)
}

/// Load, mark jobs nobody runs any more as interrupted, drop old records; `f` may change the
/// list. Written back when anything changed. Caller holds [`LOCK`].
fn load_locked(gw: &Gateway) -> Result<(Vec<LinkJob>, bool), ApiError> {
    let p = path(gw);
    let mut jobs: Vec<LinkJob> = match std::fs::read(&p) {
        Ok(b) => serde_json::from_slice(&b).map_err(internal)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(internal(e)),
    };
    let now = now_s();
    let mut changed = false;
    for j in jobs.iter_mut().filter(|j| !j.finished()) {
        if !running(|m| m.contains_key(&j.id)) {
            j.state = "failed".into();
            j.error = Some("interrupted".into());
            j.error_kind = Some("interrupted".into());
            j.updated_at = now;
            changed = true;
        }
    }
    let before = jobs.len();
    jobs.retain(|j| {
        if !j.finished() {
            return true;
        }
        let until = match (j.kind.as_str(), j.state.as_str()) {
            ("create", "done") => j.expires_at.unwrap_or(0).max(j.updated_at) + 24 * 3600,
            _ => j.updated_at + KEEP_S,
        };
        until > now
    });
    changed |= jobs.len() != before;
    Ok((jobs, changed))
}

fn save_locked(gw: &Gateway, jobs: &[LinkJob]) -> Result<(), ApiError> {
    crate::state::write_json(&path(gw), jobs).map_err(internal)
}

/// All jobs (after the restart check).
fn read(gw: &Gateway) -> Result<Vec<LinkJob>, ApiError> {
    let _g = LOCK.lock().unwrap();
    let (jobs, changed) = load_locked(gw)?;
    if changed {
        save_locked(gw, &jobs)?;
    }
    Ok(jobs)
}

/// Change the job list and write it back.
fn modify<T>(gw: &Gateway, f: impl FnOnce(&mut Vec<LinkJob>) -> T) -> Result<T, ApiError> {
    let _g = LOCK.lock().unwrap();
    let (mut jobs, _) = load_locked(gw)?;
    let r = f(&mut jobs);
    save_locked(gw, &jobs)?;
    Ok(r)
}

/// Change one job; a finished job is only changed by `force` updates (revoke).
fn update(gw: &Gateway, id: &str, f: impl FnOnce(&mut LinkJob)) -> Result<(), ApiError> {
    modify(gw, |jobs| {
        if let Some(j) = jobs.iter_mut().find(|j| j.id == id) {
            f(j);
            j.updated_at = now_s();
        }
    })
}

/// A running job moves on to `state`, unless it was cancelled meanwhile.
fn advance(gw: &Gateway, id: &str, state: &str) -> Result<(), ApiError> {
    let mut gone = false;
    update(gw, id, |j| {
        if j.finished() {
            gone = true;
        } else {
            j.state = state.into();
        }
    })?;
    if gone {
        return Err(cancelled());
    }
    Ok(())
}

fn cancelled() -> ApiError {
    err("cancelled", "the job was cancelled")
}

/// Record a new job and run `work` for it. The job is registered as running before the record
/// is written, so no reader marks it interrupted.
fn start(
    gw: &Arc<Gateway>,
    job: LinkJob,
    work: impl Future<Output = Result<(), ApiError>> + Send + 'static,
) -> Result<String, ApiError> {
    let id = job.id.clone();
    let cancel = Arc::new(Notify::new());
    let r = modify(gw, |jobs| {
        running(|m| m.insert(id.clone(), cancel.clone()));
        jobs.push(job);
    });
    if let Err(e) = r {
        running(|m| m.remove(&id));
        return Err(e);
    }
    let gw = gw.clone();
    let job_id = id.clone();
    tokio::spawn(async move {
        let r = tokio::select! {
            r = work => r,
            _ = cancel.notified() => Err(cancelled()),
        };
        if let Err(e) = r {
            fail(&gw, &job_id, &e).await;
        }
        running(|m| m.remove(&job_id));
        progress(|m| m.remove(&job_id));
    });
    Ok(id)
}

/// Mark a job failed (unless it already finished) and, for a create job that never produced a
/// link, delete its blob.
async fn fail(gw: &Arc<Gateway>, id: &str, e: &ApiError) {
    tracing::warn!(job = %id, "continue link job failed: {}", e.message);
    let mut orphan = None;
    let _ = update(gw, id, |j| {
        if !j.finished() {
            j.state = "failed".into();
            j.error = Some(e.message.clone());
            j.error_kind = Some(e.kind.clone());
        }
        if j.kind == "create" && j.link.is_none() {
            orphan = j.blob.clone().zip(j.server.clone());
        }
    });
    if let Some((blob, server)) = orphan
        && let Ok(acct) = crate::account::account(&server, &gw.state.dir)
    {
        let _ = tokio::time::timeout(Duration::from_secs(15), acct.blob_delete(&blob)).await;
    }
}

/// Removes a temporary file however the job ends.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn progress_fn(id: &str, total: u64) -> Progress {
    let id = id.to_string();
    progress(|m| m.insert(id.clone(), (0, total)));
    Arc::new(move |done| {
        progress(|m| m.insert(id.clone(), (done, total)));
    })
}

// ---------------------------------------------------------------------------------------------
// account

fn account_required() -> ApiError {
    err(
        "account_required",
        "this host is not signed in to its account server (run `vibeke login`)",
    )
}

/// This host's account, signed in. `unavailable` without an account server (no relay account
/// and no `account_url`), `account_required` without a credential.
fn host_account(gw: &Gateway) -> Result<Account, ApiError> {
    let cfg = &gw.cfg;
    let has_server =
        cfg.account_url.is_some() || (cfg.relay.is_some() && cfg.relay_token.is_none());
    if !has_server {
        return Err(ApiError::unavailable(
            "continue links need an account server; this host has none configured",
        ));
    }
    let server = crate::account::account_server(cfg);
    let acct = crate::account::account(&server, &gw.state.dir)
        .map_err(|e| ApiError::unavailable(format!("{e:#}")))?;
    match acct.credential() {
        Ok(Some(_)) => Ok(acct),
        Ok(None) => Err(account_required()),
        Err(e) => Err(internal(e)),
    }
}

fn account_err(e: vk_account::Error) -> ApiError {
    use vk_account::Error as E;
    if matches!(e, E::LoginRequired | E::Unauthorized) {
        return account_required();
    }
    match error_code(&e) {
        Some("not_found") => err(
            "not_found",
            "the bundle is not on the account server (revoked or expired)",
        ),
        Some("forbidden") => err(
            "forbidden",
            "this account may not fetch the bundle (the link is for other GitHub accounts)",
        ),
        Some("quota") => err(
            "rate_limited",
            "the account's storage for continue links is full; revoke old links",
        ),
        Some("too_large") => err("too_large", "the bundle is larger than 256 MiB"),
        Some("unknown_user") => err("unknown_user", e.to_string()),
        // The host's account left a team between the lookup and the upload (spec 20 §2.5).
        Some("not_a_member") => err(
            "unknown_team",
            "this account is no longer in one of those teams",
        ),
        Some(c @ ("not_uploaded" | "already_uploaded" | "size_mismatch")) => {
            err("conflict", format!("account server: {c}"))
        }
        _ => ApiError::unavailable(e.to_string()),
    }
}

// ---------------------------------------------------------------------------------------------
// dispatch

/// The continue-link methods; `None` for any other method. `handoff.cancel` is answered here
/// only when its `id` names one of this host's link jobs (the server keeps the send jobs).
pub async fn dispatch(
    gw: &Arc<Gateway>,
    dev: &Device,
    method: &str,
    p: &Value,
) -> Option<ApiResult> {
    Some(match method {
        "handoff.link.create" => create(gw, dev, p).await,
        "handoff.link.status" => status(gw, p),
        "handoff.link.list" => list(gw),
        "handoff.link.revoke" => revoke(gw, p).await,
        "handoff.link.redeem" => redeem(gw, dev, p).await,
        "handoff.link.cancel" => cancel(gw, s(p, "job").or(s(p, "id")).unwrap_or("")),
        "handoff.cancel" => {
            let id = s(p, "id").filter(|id| id.starts_with(JOB_PREFIX))?;
            cancel(gw, id)
        }
        _ => return None,
    })
}

fn view(j: &LinkJob) -> Value {
    let mut v = json!({"job": j.id, "kind": j.kind, "state": j.state,
                       "created_at": j.created_at, "updated_at": j.updated_at});
    if let Some((done, total)) = progress(|m| m.get(&j.id).copied()) {
        v["progress"] = json!({"done": done, "total": total});
    }
    let opt = |v: &mut Value, k: &str, x: Option<Value>| {
        if let Some(x) = x {
            v[k] = x;
        }
    };
    opt(&mut v, "pane", j.pane.clone().map(Value::from));
    opt(&mut v, "error", j.error.clone().map(Value::from));
    opt(&mut v, "error_kind", j.error_kind.clone().map(Value::from));
    opt(&mut v, "link", j.link.clone().map(Value::from));
    opt(&mut v, "markdown", j.markdown.clone().map(Value::from));
    opt(&mut v, "blob", j.blob.clone().map(Value::from));
    opt(&mut v, "expires_at", j.expires_at.map(Value::from));
    opt(&mut v, "size", j.size.map(Value::from));
    opt(&mut v, "incoming", j.incoming.clone().map(Value::from));
    opt(
        &mut v,
        "incoming_state",
        j.incoming_state.clone().map(Value::from),
    );
    opt(&mut v, "from", j.sender.clone());
    opt(
        &mut v,
        "brief",
        j.brief.as_ref().and_then(|b| serde_json::to_value(b).ok()),
    );
    if j.revoked {
        v["revoked"] = true.into();
    }
    v
}

fn status(gw: &Gateway, p: &Value) -> ApiResult {
    let id = s(p, "job").or(s(p, "id")).unwrap_or("");
    if id.is_empty() {
        return Err(ApiError::invalid("job is required"));
    }
    read(gw)?
        .iter()
        .find(|j| j.id == id)
        .map(view)
        .ok_or_else(|| err("not_found", format!("no continue link job {id}")))
}

fn list(gw: &Gateway) -> ApiResult {
    let now = now_s();
    let links: Vec<Value> = read(gw)?
        .iter()
        .filter(|j| j.kind == "create" && j.state == "done" && !j.revoked)
        .filter(|j| j.expires_at.is_some_and(|e| e > now))
        .map(|j| {
            json!({"blob": j.blob, "job": j.id, "pane": j.pane, "brief": j.brief,
                   "created_at": j.created_at, "expires_at": j.expires_at, "size": j.size,
                   "link": j.link, "markdown": j.markdown})
        })
        .collect();
    Ok(json!({"links": links}))
}

fn cancel(gw: &Gateway, id: &str) -> ApiResult {
    if id.is_empty() {
        return Err(ApiError::invalid("job is required"));
    }
    let mut found = None;
    modify(gw, |jobs| {
        if let Some(j) = jobs.iter_mut().find(|j| j.id == id) {
            if !j.finished() {
                j.state = "failed".into();
                j.error = Some("cancelled".into());
                j.error_kind = Some("cancelled".into());
                j.updated_at = now_s();
            }
            found = Some(j.clone());
        }
    })?;
    let Some(j) = found else {
        return Err(err("not_found", format!("no continue link job {id}")));
    };
    if let Some(n) = running(|m| m.get(id).cloned()) {
        // A stored permit: the job stops even if it is not waiting yet.
        n.notify_one();
    }
    Ok(json!({"job": view(&j)}))
}

fn new_job(kind: &str) -> LinkJob {
    let now = now_s();
    LinkJob {
        id: format!(
            "{JOB_PREFIX}{}",
            ulid::Ulid::generate().to_string().to_lowercase()
        ),
        kind: kind.into(),
        created_at: now,
        updated_at: now,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------------------------
// create

async fn create(gw: &Arc<Gateway>, dev: &Device, p: &Value) -> ApiResult {
    let pane = req(p, "pane")?.to_string();
    let interrupt = p.get("interrupt").and_then(Value::as_bool) == Some(true);
    let ttl_s = match p.get("ttl_s").filter(|v| !v.is_null()) {
        None => DEFAULT_TTL_S,
        Some(v) => v
            .as_u64()
            .filter(|t| (MIN_TTL_S..=MAX_TTL_S).contains(t))
            .ok_or_else(|| {
                ApiError::invalid(format!(
                    "ttl_s is between {MIN_TTL_S} and {MAX_TTL_S} seconds"
                ))
            })?,
    };
    let github = match s(p, "access") {
        Some("link") => false,
        Some("github") => true,
        _ => return Err(ApiError::invalid("access is link or github")),
    };
    let teams = crate::api::teams_param(p)?;
    if !github && teams.is_some() {
        return Err(ApiError::invalid("teams is for access github"));
    }
    let acct = host_account(gw)?;
    let access = if github {
        // Strict: a malformed or empty `github` is refused, never the account's whole list.
        let logins: Vec<String> = crate::api::github_param(p)?.unwrap_or_default();
        if logins.iter().any(|l| l == "*") {
            if teams.is_some() {
                return Err(ApiError::invalid(
                    "\"*\" (any GitHub account) already includes every team",
                ));
            }
            // Every account on the account server is a GitHub account.
            BlobAccess::Any
        } else {
            let mut ids = Vec::new();
            let mut team_ids = Vec::new();
            if logins.is_empty() && teams.is_none() {
                // No list: the people and teams on the account page.
                let sharing = acct.sharing().await.map_err(account_err)?;
                if sharing.people.is_empty() && sharing.teams.is_empty() {
                    return Err(crate::api::nobody_to_share_with(acct.server()));
                }
                if sharing.people.len() > crate::api::MAX_DEFAULT_PEOPLE {
                    return Err(ApiError::invalid(format!(
                        "your sharing list has more than {} people",
                        crate::api::MAX_DEFAULT_PEOPLE
                    )));
                }
                ids = sharing.people.into_iter().map(|u| u.id).collect();
                team_ids = sharing
                    .teams
                    .into_iter()
                    .map(|t| t.id)
                    .take(crate::api::MAX_TEAMS)
                    .collect();
            }
            for login in &logins {
                let u = acct.github_user(login).await.map_err(|e| {
                    if error_code(&e) == Some("unknown_user") {
                        err("unknown_user", format!("{login} is not a GitHub user"))
                    } else {
                        account_err(e)
                    }
                })?;
                ids.push(u.id);
            }
            if let Some(names) = &teams {
                let mine = acct.teams().await.map_err(account_err)?;
                team_ids = crate::api::pick_teams(names, &mine)?
                    .into_iter()
                    .map(|t| t.id)
                    .collect();
            }
            BlobAccess::Github {
                ids,
                teams: team_ids,
            }
        }
    } else {
        BlobAccess::Any
    };
    let login = acct
        .credential()
        .ok()
        .flatten()
        .map(|c| c.login)
        .unwrap_or_default();
    let mut job = new_job("create");
    job.state = "exporting".into();
    job.pane = Some(pane.clone());
    job.server = Some(acct.server().to_string());
    let id = job.id.clone();
    // Who the link is for, echoed so a caller can tell this gateway applied `teams` (an older
    // one ignores the field).
    let echo = match &access {
        BlobAccess::Github { ids, teams } => {
            json!({"access": "github", "people": ids.len(), "teams": teams})
        }
        _ => json!({"access": if github { "github" } else { "link" }}),
    };
    let work = run_create(
        gw.clone(),
        dev.clone(),
        id.clone(),
        acct,
        CreateInput {
            pane,
            interrupt,
            ttl_s,
            access,
            login,
        },
    );
    let id = start(gw, job, work)?;
    gw.state.audit(
        &json!({"ts": now_s(), "event": "handoff.link.create", "device": dev.id, "job": id}),
    );
    let mut out = json!({"job": id});
    for (k, v) in echo.as_object().into_iter().flatten() {
        out[k] = v.clone();
    }
    Ok(out)
}

struct CreateInput {
    pane: String,
    interrupt: bool,
    ttl_s: u64,
    access: BlobAccess,
    /// The sender's login on the account server.
    login: String,
}

async fn run_create(
    gw: Arc<Gateway>,
    dev: Device,
    id: String,
    acct: Account,
    input: CreateInput,
) -> Result<(), ApiError> {
    // The server bridge's owner is not a registered device; the server authorized it.
    let auth = (!dev.public.is_empty()).then_some(&dev);
    let ex = export_bundle(
        &gw,
        &format!("gateway:{}", dev.name),
        auth,
        &input.pane,
        input.interrupt,
        false,
        Some(&id),
        None,
    )
    .await?;
    let _plain = Cleanup(ex.path.clone());
    let key = stream::new_key();
    let enc = dir(&gw)?.join(format!("{id}.vkc"));
    let _enc = Cleanup(enc.clone());
    let (src, dst) = (ex.path.clone(), enc.clone());
    let sealed = blocking(move || stream::encrypt_file(&src, &dst, &key)).await?;
    if sealed.sha256 != ex.sha256 || sealed.size != ex.size {
        return Err(err("conflict", "the bundle changed while it was encrypted"));
    }
    if sealed.cipher_size > MAX_BLOB {
        return Err(err("too_large", "the bundle is larger than 256 MiB"));
    }
    advance(&gw, &id, "uploading")?;
    let created = acct
        .blob_create(sealed.cipher_size, Some(input.ttl_s), input.access)
        .await
        .map_err(account_err)?;
    update(&gw, &id, |j| {
        j.blob = Some(created.id.clone());
        j.expires_at = Some(created.expires_at);
        j.size = Some(ex.size);
    })?;
    acct.blob_upload_with(
        &created.id,
        &enc,
        Some(progress_fn(&id, sealed.cipher_size)),
    )
    .await
    .map_err(account_err)?;

    let brief = brief_of(&gw, &input.pane, &ex.manifest, &input.login).await;
    let mut link = ContinueLink::new(
        acct.server(),
        &created.id,
        &key,
        &ex.sha256,
        ex.size,
        created.expires_at,
        brief,
    );
    let app = gw
        .cfg
        .app_url
        .clone()
        .unwrap_or_else(|| DEFAULT_APP_URL.to_string());
    let url = link.fit(&app)?;
    let md = markdown(&link.brief, &url, created.expires_at);
    let mut gone = false;
    update(&gw, &id, |j| {
        if j.finished() {
            gone = true;
            return;
        }
        j.state = "done".into();
        j.link = Some(url);
        j.markdown = Some(md);
        j.brief = Some(link.brief.clone());
    })?;
    if gone {
        return Err(cancelled());
    }
    gw.state.audit(
        &json!({"ts": now_s(), "event": "handoff.link.created", "job": id,
                           "blob": created.id, "size": ex.size, "expires_at": created.expires_at}),
    );
    Ok(())
}

/// What the receiving screen shows: repository, branch, harness, a title, the agent's last
/// message and the sender's login.
async fn brief_of(gw: &Gateway, pane: &str, m: &Manifest, login: &str) -> Brief {
    let info = gw
        .server
        .call("pane.get", json!({"pane": pane}))
        .await
        .unwrap_or(Value::Null);
    let mut run = info.get("run").cloned().unwrap_or(Value::Null);
    normalize(&mut run);
    let pane_v = info.get("pane").cloned().unwrap_or(Value::Null);
    let title = [
        s(&run, "title"),
        s(&run, "name"),
        s(&pane_v, "title"),
        s(&pane_v, "auto_title"),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .find(|t| !t.is_empty())
    .map(str::to_string);
    Brief {
        repo: m.repo_name.clone(),
        branch: m.branch.clone(),
        harness: m.harness.clone(),
        title,
        last: m.last_message.clone(),
        from: login.to_string(),
    }
    .bounded()
}

fn harness_name(h: &str) -> String {
    match h {
        "claude" => "Claude Code".into(),
        "codex" => "Codex".into(),
        "opencode" => "OpenCode".into(),
        "gemini" => "Gemini CLI".into(),
        "pi" => "pi".into(),
        other => other.to_string(),
    }
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `15 Jan` for unix seconds (UTC).
pub fn day_month(unix: u64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = (unix / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{d} {}", MONTHS[(m - 1) as usize])
}

/// The pull-request snippet (spec 20 §4.6).
pub fn markdown(b: &Brief, url: &str, expires_at: u64) -> String {
    let tick = |x: &str| x.replace('`', "'");
    let who = b
        .harness
        .as_deref()
        .map(harness_name)
        .unwrap_or_else(|| "An agent session".into());
    let on = match (&b.branch, b.repo.is_empty()) {
        (Some(br), _) => format!(" on `{}`", tick(br)),
        (None, false) => format!(" in `{}`", tick(&b.repo)),
        (None, true) => String::new(),
    };
    let by = if b.from.is_empty() {
        String::new()
    } else {
        format!(", sent by @{}", b.from)
    };
    format!(
        "**Continue this session**: {who}{on}{by}, link valid until {}.\n[Open in Vibeke]({url})",
        day_month(expires_at)
    )
}

// ---------------------------------------------------------------------------------------------
// redeem

async fn redeem(gw: &Arc<Gateway>, dev: &Device, p: &Value) -> ApiResult {
    let link = ContinueLink::parse(req(p, "link")?)?;
    if link.exp <= now_s() {
        return Err(err("expired", "the continue link has expired"));
    }
    if link.size > vk_handoff::MAX_BUNDLE {
        return Err(err("too_large", "the linked bundle exceeds 200 MiB"));
    }
    let acct = host_account(gw)?;
    // The link names the server; this host's token only goes to its own account server.
    let theirs = vk_account::server_origin(&link.server)
        .map_err(|_| ApiError::invalid("the link's account server is not a valid origin"))?;
    if theirs != acct.server() {
        return Err(err(
            "forbidden",
            format!(
                "the link is for the account server {theirs}; this host signs in to {}",
                acct.server()
            ),
        ));
    }
    let mut job = new_job("redeem");
    job.state = "downloading".into();
    job.blob = Some(link.blob.clone());
    job.server = Some(theirs);
    job.expires_at = Some(link.exp);
    job.size = Some(link.size);
    job.brief = Some(link.brief.bounded());
    let id = job.id.clone();
    let work = run_redeem(gw.clone(), dev.clone(), id.clone(), acct, link);
    let id = start(gw, job, work)?;
    gw.state.audit(
        &json!({"ts": now_s(), "event": "handoff.link.redeem", "device": dev.id, "job": id}),
    );
    Ok(json!({"job": id}))
}

async fn run_redeem(
    gw: Arc<Gateway>,
    dev: Device,
    id: String,
    acct: Account,
    link: ContinueLink,
) -> Result<(), ApiError> {
    let key = link.key()?;
    // Who uploaded it, as the account server authenticated them (not what the link claims).
    let meta = acct.blob_meta(&link.blob).await.map_err(account_err)?;
    let expect = stream::cipher_len(link.size);
    if meta.size != expect {
        return Err(err(
            "conflict",
            "the bundle on the account server does not match the link (size)",
        ));
    }
    let folder = dir(&gw)?;
    let enc = folder.join(format!("{id}.in.vkc"));
    let _enc = Cleanup(enc.clone());
    acct.blob_download_with(&link.blob, &enc, expect, Some(progress_fn(&id, expect)))
        .await
        .map_err(account_err)?;
    advance(&gw, &id, "importing")?;
    let plain = folder.join(format!("{id}.in.tar.zst"));
    let _plain = Cleanup(plain.clone());
    let (src, dst, max) = (enc.clone(), plain.clone(), link.size);
    let opened = tokio::task::spawn_blocking(move || stream::decrypt_file(&src, &dst, &key, max))
        .await
        .map_err(internal)??;
    if opened.size != link.size || opened.sha256 != link.sha256 {
        return Err(err(
            "conflict",
            "checksum mismatch: the bundle is not the one the link names",
        ));
    }
    let p2 = plain.clone();
    let manifest = blocking(move || vk_handoff::read_manifest(&p2)).await?;
    let sender = json!({"id": meta.from.id, "login": meta.from.login});
    let mut from = json!({
        "host": format!("link:{}", link.blob),
        "owner": "teammate",
        "user": meta.from.login,
        "account": sender.clone(),
    });
    let name = vk_handoff::clean(&link.brief.from, 100);
    if !name.is_empty() {
        from["name"] = name.into();
    }
    let rec = deliver_to_server(
        &gw,
        &format!("gateway:{}", dev.name),
        &plain,
        &manifest,
        &opened.sha256,
        &from,
    )
    .await?;
    let incoming = s(&rec, "id").map(str::to_string);
    let incoming_state = s(&rec, "state").map(str::to_string);
    let mut gone = false;
    update(&gw, &id, |j| {
        if j.finished() {
            gone = true;
            return;
        }
        j.state = "done".into();
        j.incoming = incoming.clone();
        j.incoming_state = incoming_state;
        j.sender = Some(sender);
    })?;
    if gone {
        return Err(cancelled());
    }
    gw.state.audit(
        &json!({"ts": now_s(), "event": "handoff.link.redeemed", "job": id,
                           "blob": link.blob, "from": from, "incoming": incoming}),
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// revoke

async fn revoke(gw: &Arc<Gateway>, p: &Value) -> ApiResult {
    let blob = req(p, "blob")?.to_string();
    if !valid_blob_id(&blob) {
        return Err(ApiError::invalid("blob is not a blob id"));
    }
    // A job still uploading this blob stops first.
    let jobs = read(gw)?;
    for j in jobs
        .iter()
        .filter(|j| j.kind == "create" && j.blob.as_deref() == Some(&blob) && !j.finished())
    {
        let _ = cancel(gw, &j.id);
    }
    let known = jobs
        .iter()
        .any(|j| j.kind == "create" && j.blob.as_deref() == Some(&blob));
    let acct = host_account(gw)?;
    match acct.blob_delete(&blob).await {
        Ok(()) => {}
        // Already gone on the server: still forget it here.
        Err(e) if known && error_code(&e) == Some("not_found") => {}
        Err(e) => return Err(account_err(e)),
    }
    modify(gw, |jobs| {
        for j in jobs
            .iter_mut()
            .filter(|j| j.kind == "create" && j.blob.as_deref() == Some(&blob))
        {
            j.revoked = true;
            j.updated_at = now_s();
        }
    })?;
    gw.state
        .audit(&json!({"ts": now_s(), "event": "handoff.link.revoked", "blob": blob}));
    Ok(json!({}))
}

/// The job file, for tests.
#[cfg(test)]
fn job_file(gw: &Gateway) -> PathBuf {
    path(gw)
}

#[cfg(test)]
#[path = "handoff_link_tests.rs"]
mod tests;
