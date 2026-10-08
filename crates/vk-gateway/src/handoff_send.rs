//! Gateway-to-gateway handoff delivery, source side (spec 16 §15.2). The server keeps outgoing
//! jobs (`handoff.send` creates one and announces it with a `handoff.job` event); this worker
//! runs them:
//!
//! 1. `handoff.job.update {state: exporting}`, then export the pane's work at a turn boundary
//!    ([`crate::handoff::export_bundle`], interrupting the agent only when the job says so);
//! 2. connect to the peer (`peers.json`) with backoff, `handoff.offer`, and stream 1 MiB binary
//!    chunks with `handoff.write` from the offset the peer reports;
//! 3. `handoff.commit`, then `handoff.job.update {state: delivered, incoming_state}`.
//!
//! A dropped connection is redialed with backoff; `handoff.status` gives the offset to continue
//! from (the offer is idempotent, so a forgotten upload restarts cleanly). Progress goes to the
//! server about four times a second. A cancelled job (the server refuses further updates) stops
//! the worker, which discards the upload on the peer. Jobs left `exporting`/`sending` by a
//! previous gateway process are picked up again at start and every 30 s.
//!
//! The worker also publishes this host's peers to the server (`handoff.peers.set`), the list
//! clients pick a destination from.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use crate::Gateway;
use crate::api::ApiError;
use crate::events::Fanout;
use crate::handoff::{Expected, Exported, blocking, export_bundle};
use crate::handoff_peer::CHUNK;
use crate::peer_client::{Backoff, Conn, PeerClient, is_refusal};
use crate::state::PeerRecord;

/// One connection attempt series (full-jitter backoff) before the stall check runs again.
pub const CONNECT_DEADLINE: Duration = Duration::from_secs(120);
/// A job fails when no byte moved for this long.
pub const STALL_LIMIT: Duration = Duration::from_secs(10 * 60);
/// At most this often a progress update goes to the server (the last one always does).
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// Re-list the server's jobs and re-publish peers this often (missed events, restarts, peers
/// added by `vibeke-gateway peer add` in another process).
const RESCAN: Duration = Duration::from_secs(30);
/// Fresh offers per job (after checksum mismatches or forgotten uploads) before giving up.
const MAX_OFFERS: u32 = 4;

/// Jobs this process is running, with their cancel flags.
static RUNNING: Mutex<Option<HashMap<String, Arc<AtomicBool>>>> = Mutex::new(None);
/// The peer list last published to the server, per host id.
static PUBLISHED: Mutex<Option<HashMap<String, Value>>> = Mutex::new(None);

fn running<T>(f: impl FnOnce(&mut HashMap<String, Arc<AtomicBool>>) -> T) -> T {
    let mut g = RUNNING.lock().unwrap();
    f(g.get_or_insert_with(HashMap::new))
}

fn s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(|v| v.as_str())
}

fn cancelled() -> ApiError {
    ApiError::new("cancelled", "the handoff was cancelled")
}

/// Run outgoing jobs until the process stops.
pub async fn run(gw: Arc<Gateway>) {
    let mut rx = gw.hub.subscribe();
    publish_peers(&gw, true).await;
    scan(&gw).await;
    let mut tick = tokio::time::interval(RESCAN);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(Fanout::Event(e)) => {
                    if s(&e, "type") == Some("handoff.job")
                        && let Some(job) = e.get("data")
                    {
                        on_job(&gw, job);
                    }
                }
                Ok(Fanout::Reset) | Err(RecvError::Lagged(_)) => scan(&gw).await,
                Err(RecvError::Closed) => return,
            },
            _ = tick.tick() => {
                publish_peers(&gw, false).await;
                scan(&gw).await;
            }
        }
    }
}

/// The peers as clients see them (no keys, no addresses).
pub fn peers_json(peers: &[PeerRecord]) -> Value {
    Value::Array(
        peers
            .iter()
            .map(|p| {
                json!({"id": p.id, "name": p.name, "owner": p.owner, "added_at": p.added_at,
                       "expires_at": p.expires_at, "expired": p.expired()})
            })
            .collect(),
    )
}

/// Tell the server which peers this host can send to (when they changed, or always with
/// `force`). Called at start, periodically, and after `peer.redeem` / `peer.remove`.
pub async fn publish_peers(gw: &Gateway, force: bool) {
    let Ok(peers) = gw.state.peers() else {
        return;
    };
    let list = peers_json(&peers);
    let host = gw.keys.host_id();
    let unchanged = PUBLISHED
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .get(&host)
        == Some(&list);
    if !force && unchanged {
        return;
    }
    match gw
        .server
        .call("handoff.peers.set", json!({"peers": list}))
        .await
    {
        Ok(_) => {
            PUBLISHED
                .lock()
                .unwrap()
                .get_or_insert_with(HashMap::new)
                .insert(host, list);
        }
        Err(e) => tracing::debug!("handoff.peers.set: {}", e.message),
    }
}

/// A `handoff.job` event: start a queued job, or flag a running one as cancelled.
fn on_job(gw: &Arc<Gateway>, job: &Value) {
    let Some(id) = s(job, "id") else { return };
    match s(job, "state") {
        Some("queued") => start(gw, job.clone()),
        Some("cancelled") => {
            if let Some(flag) = running(|m| m.get(id).cloned()) {
                flag.store(true, Ordering::SeqCst);
            }
        }
        _ => {}
    }
}

/// Start every job that should be running and isn't (queued, or left in flight by a previous
/// gateway process).
async fn scan(gw: &Arc<Gateway>) {
    let Ok(r) = gw.server.call("handoff.jobs", json!({})).await else {
        return;
    };
    for job in r
        .get("jobs")
        .and_then(|j| j.as_array())
        .into_iter()
        .flatten()
    {
        if matches!(s(job, "state"), Some("queued" | "exporting" | "sending")) {
            start(gw, job.clone());
        }
    }
}

fn start(gw: &Arc<Gateway>, job: Value) {
    let Some(id) = s(&job, "id").map(str::to_string) else {
        return;
    };
    let flag = Arc::new(AtomicBool::new(false));
    let fresh = running(|m| {
        if m.contains_key(&id) {
            false
        } else {
            m.insert(id.clone(), flag.clone());
            true
        }
    });
    if !fresh {
        return;
    }
    let gw = gw.clone();
    tokio::spawn(async move {
        run_job(&gw, &job, &flag).await;
        running(|m| m.remove(&id));
    });
}

/// Reports a job's progress and outcome to the server.
struct Reporter<'a> {
    gw: &'a Gateway,
    id: String,
    cancel: &'a AtomicBool,
    last: Option<Instant>,
}

impl Reporter<'_> {
    /// `handoff.job.update`. A refusal (`conflict`: the job was cancelled or already finished,
    /// `not_found`: removed) stops the job; an unreachable server doesn't.
    async fn update(&mut self, mut fields: Value) -> Result<(), ApiError> {
        fields["id"] = self.id.clone().into();
        match self.gw.server.call("handoff.job.update", fields).await {
            Ok(_) => Ok(()),
            Err(e) if matches!(e.kind.as_str(), "conflict" | "not_found") => {
                self.cancel.store(true, Ordering::SeqCst);
                Err(cancelled())
            }
            Err(e) => {
                tracing::debug!(job = %self.id, "handoff.job.update: {}", e.message);
                Ok(())
            }
        }
    }

    async fn progress(&mut self, sent: u64, total: u64) -> Result<(), ApiError> {
        if sent < total && self.last.is_some_and(|t| t.elapsed() < PROGRESS_EVERY) {
            return Ok(());
        }
        self.last = Some(Instant::now());
        self.update(json!({"sent": sent, "total": total})).await
    }
}

async fn run_job(gw: &Arc<Gateway>, job: &Value, cancel: &AtomicBool) {
    let id = s(job, "id").unwrap_or("").to_string();
    let mut rep = Reporter {
        gw: gw.as_ref(),
        id: id.clone(),
        cancel,
        last: None,
    };
    let r = send(gw, job, cancel, &mut rep).await;
    match r {
        Ok(rec) => {
            let mut done = json!({"state": "delivered", "incoming_state": s(&rec, "state")});
            if let Some(inc) = s(&rec, "incoming") {
                done["incoming"] = inc.into();
            }
            let _ = rep.update(done).await;
            gw.state.audit(
                &json!({"ts": crate::state::now_s(), "event": "handoff.sent", "job": id,
                                   "peer": s(job, "peer"), "incoming": rec.get("incoming")}),
            );
        }
        Err(_) if cancel.load(Ordering::SeqCst) => {
            tracing::info!(job = %id, "handoff cancelled");
        }
        Err(e) => {
            tracing::warn!(job = %id, "handoff failed: {}", e.message);
            let _ = rep
                .update(json!({"state": "failed", "error": e.message}))
                .await;
        }
    }
}

fn find_peer(gw: &Gateway, want: &str) -> Result<PeerRecord, ApiError> {
    let peers = gw
        .state
        .peers()
        .map_err(|e| ApiError::new("internal", e.to_string()))?;
    let rec = peers
        .iter()
        .find(|p| p.id == want)
        .or_else(|| peers.iter().find(|p| p.name == want))
        .cloned()
        .ok_or_else(|| ApiError::new("not_found", format!("this host has no peer {want}")))?;
    if rec.expired() {
        return Err(ApiError::new(
            "forbidden",
            format!("access to {} has expired", rec.name),
        ));
    }
    Ok(rec)
}

/// Removes the exported bundle however the job ends.
struct Cleanup(std::path::PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn send(
    gw: &Arc<Gateway>,
    job: &Value,
    cancel: &AtomicBool,
    rep: &mut Reporter<'_>,
) -> Result<Value, ApiError> {
    let id = s(job, "id").unwrap_or("");
    let pane = s(job, "pane").ok_or_else(|| ApiError::invalid("job without a pane"))?;
    let peer = s(job, "peer").ok_or_else(|| ApiError::invalid("job without a peer"))?;
    let interrupt = job.get("interrupt").and_then(|v| v.as_bool()) == Some(true);
    rep.update(json!({"state": "exporting"})).await?;
    let rec = find_peer(gw, peer)?;
    // A send approved from a pane (`auth.approve`): deliver only what the user approved. The
    // export is pinned to the approved commit and re-checks HEAD and the branch after packing.
    let expect = job.get("expect").and_then(Expected::from_json);
    let ex = export_bundle(
        gw,
        &format!("gateway:handoff {id}"),
        None,
        pane,
        interrupt,
        false,
        Some(id),
        expect.as_ref(),
    )
    .await?;
    let _cleanup = Cleanup(ex.path.clone());
    if let Some(e) = &expect {
        check_expected(e, &ex.manifest)?;
    }
    if cancel.load(Ordering::SeqCst) {
        return Err(cancelled());
    }
    rep.update(json!({"state": "sending", "sent": 0, "total": ex.size}))
        .await?;
    transfer(&rec, &ex, cancel, rep).await
}

/// The export of an approved send must come from the repository, branch and commit the user
/// approved (recorded when the pane asked); otherwise the job fails and nothing is sent.
fn check_expected(expect: &Expected, m: &vk_handoff::Manifest) -> Result<(), ApiError> {
    expect.check(&m.source_root, m.branch.as_deref(), &m.head)
}

/// What one step of the transfer did.
enum Step {
    /// Committed: the peer's record of the incoming handoff.
    Done(Value),
    /// Bytes moved (or an upload was accepted).
    Moved,
    /// Nothing moved; go round again.
    Again,
}

/// Where the transfer stands.
struct Upload {
    /// The peer's upload id, once offered.
    id: Option<String>,
    /// Bytes the peer has.
    offset: u64,
    /// Ask the peer where it stands before the next write or commit.
    resync: bool,
    offers: u32,
}

/// Stream `ex` to `rec` until it is committed, the job is cancelled, the peer refuses, or nothing
/// moves for [`STALL_LIMIT`].
async fn transfer(
    rec: &PeerRecord,
    ex: &Exported,
    cancel: &AtomicBool,
    rep: &mut Reporter<'_>,
) -> Result<Value, ApiError> {
    let mut conn: Option<Conn> = None;
    let mut up = Upload {
        id: None,
        offset: 0,
        resync: false,
        offers: 0,
    };
    let mut moved = Instant::now();
    let mut backoff = Backoff::default();
    let r = loop {
        if cancel.load(Ordering::SeqCst) {
            break Err(cancelled());
        }
        if moved.elapsed() > STALL_LIMIT {
            break Err(ApiError::new(
                "timeout",
                format!("nothing reached {} for 10 minutes", rec.name),
            ));
        }
        if conn.is_none() {
            match PeerClient::connect_with_backoff(rec, CONNECT_DEADLINE).await {
                Ok(mut c) => {
                    crate::peers::renew_ticket(&rep.gw.state, rec, &mut c).await;
                    conn = Some(c)
                }
                Err(e) if is_refusal(&e) => {
                    break Err(ApiError::new(
                        "forbidden",
                        format!("{} refused this host: {e:#}", rec.name),
                    ));
                }
                Err(e) => {
                    tracing::info!(peer = %rec.name, "handoff: can't reach the peer yet: {e:#}");
                    continue;
                }
            }
        }
        let c = conn.as_mut().expect("connected");
        match step(c, ex, &mut up).await {
            Ok(Step::Done(v)) => break Ok(v),
            Ok(Step::Moved) => {
                moved = Instant::now();
                backoff.reset();
                if let Err(e) = rep.progress(up.offset, ex.size).await {
                    break Err(e);
                }
            }
            Ok(Step::Again) => tokio::time::sleep(Duration::from_millis(500)).await,
            Err(e) if e.kind == "unavailable" => {
                // The connection broke (or the peer's server is briefly away): redial and ask
                // the peer where the upload stands.
                tracing::info!(peer = %rec.name, "handoff transfer interrupted, resuming: {}", e.message);
                if let Some(c) = conn.take() {
                    c.close().await;
                }
                up.resync = up.id.is_some();
                tokio::time::sleep(backoff.next_delay()).await;
            }
            Err(e) => break Err(e),
        }
    };
    if r.is_err()
        && cancel.load(Ordering::SeqCst)
        && let Some(id) = up.id.clone()
    {
        // Best effort: free the peer's copy now rather than in 24 h.
        let discard = async {
            let mut c = match conn.take() {
                Some(c) => c,
                None => PeerClient::connect(rec).await.ok()?,
            };
            let _ = c.call("handoff.discard", json!({"id": id})).await;
            c.close().await;
            Some(())
        };
        let _ = tokio::time::timeout(Duration::from_secs(15), discard).await;
    }
    if let Some(c) = conn {
        c.close().await;
    }
    r
}

async fn step(c: &mut Conn, ex: &Exported, up: &mut Upload) -> Result<Step, ApiError> {
    if up.resync {
        let Some(id) = up.id.clone() else {
            up.resync = false;
            return Ok(Step::Again);
        };
        return match c.call("handoff.status", json!({"id": id})).await {
            Ok(st) => match s(&st, "state") {
                Some("committed") => Ok(Step::Done(st.get("result").cloned().unwrap_or_default())),
                // A commit from before the break is still running there.
                Some("committing") => Ok(Step::Again),
                _ => {
                    up.resync = false;
                    up.offset = st
                        .get("received")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0)
                        .min(ex.size);
                    Ok(Step::Again)
                }
            },
            Err(e) if e.kind == "not_found" => {
                // The peer forgot the upload (restart, expiry): offer again.
                up.resync = false;
                up.id = None;
                up.offset = 0;
                Ok(Step::Again)
            }
            Err(e) => Err(e),
        };
    }
    let Some(id) = up.id.clone() else {
        up.offers += 1;
        if up.offers > MAX_OFFERS {
            return Err(ApiError::new(
                "conflict",
                "the peer kept rejecting the bundle",
            ));
        }
        let r = c
            .call(
                "handoff.offer",
                json!({"manifest": ex.manifest, "size": ex.size, "sha256": ex.sha256}),
            )
            .await?;
        if r.get("committed").and_then(|v| v.as_bool()) == Some(true) {
            return Ok(Step::Done(r.get("result").cloned().unwrap_or_default()));
        }
        up.id = Some(
            s(&r, "id")
                .ok_or_else(|| ApiError::new("internal", "handoff.offer returned no id"))?
                .to_string(),
        );
        up.offset = r
            .get("received")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(ex.size);
        return Ok(Step::Moved);
    };
    if up.offset < ex.size {
        let (path, at) = (ex.path.clone(), up.offset);
        let chunk = blocking(move || {
            let mut f = std::fs::File::open(&path)?;
            f.seek(SeekFrom::Start(at))?;
            let mut buf = Vec::with_capacity(CHUNK);
            f.take(CHUNK as u64).read_to_end(&mut buf)?;
            Ok(buf)
        })
        .await?;
        if chunk.is_empty() {
            return Err(ApiError::new(
                "internal",
                "the bundle is shorter than its size",
            ));
        }
        return match c
            .call_with_payload("handoff.write", json!({"id": id, "offset": at}), &chunk)
            .await
        {
            Ok(r) => {
                up.offset = r
                    .get("received")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(at + chunk.len() as u64)
                    .min(ex.size);
                Ok(Step::Moved)
            }
            Err(e) if matches!(e.kind.as_str(), "conflict" | "not_found") => {
                up.resync = true;
                Ok(Step::Again)
            }
            Err(e) => Err(e),
        };
    }
    match c.call("handoff.commit", json!({"id": id})).await {
        Ok(r) => Ok(Step::Done(r)),
        Err(e) if e.message.contains("checksum") => {
            up.id = None;
            up.offset = 0;
            Ok(Step::Again)
        }
        Err(e) if matches!(e.kind.as_str(), "conflict" | "not_found") => {
            up.resync = true;
            Ok(Step::Again)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_approved_send_delivers_only_what_was_approved() {
        let m = vk_handoff::Manifest {
            source_root: "/src/app".into(),
            branch: Some("main".into()),
            head: "a".repeat(40),
            ..Default::default()
        };
        let expect = json!({"repo_root": "/src/app", "branch": "main", "head": "a".repeat(40),
                            "request": "ap-1", "requested_by": "p1", "approved_by": "tui"});
        let exp = |v: &Value| Expected::from_json(v).expect("an expect object");
        assert!(check_expected(&exp(&expect), &m).is_ok());
        assert!(Expected::from_json(&Value::Null).is_none());
        for (k, v) in [
            ("repo_root", json!("/src/other")),
            ("branch", json!("feature")),
            ("branch", Value::Null),
            ("head", json!("b".repeat(40))),
        ] {
            let mut e = expect.clone();
            e[k] = v;
            let err = check_expected(&exp(&e), &m).unwrap_err();
            assert_eq!(err.kind, "conflict");
            assert!(err.message.starts_with("repo_moved"), "{}", err.message);
        }
    }
}
