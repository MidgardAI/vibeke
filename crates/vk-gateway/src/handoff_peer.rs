//! Gateway-to-gateway handoff delivery, destination side (spec 16 §15.2). Another host (a `peer`
//! device) offers a bundle, streams it in chunks and commits it; the bundle then goes to the
//! server as an incoming handoff (`handoff.incoming.add`), where the receiver, or the
//! auto-import policy, decides where the work lands.
//!
//! - `handoff.offer {manifest, size, sha256}` → `{id, received, size}`. Idempotent per (device,
//!   sha256): offering the same bundle again returns the same id and the bytes already received,
//!   so a sender that lost its connection resumes instead of starting over. An offer of a bundle
//!   committed in the last 24 h answers `{id, received: size, committed: true, result}`.
//! - `handoff.status {id}` → `{id, received, size, state: receiving|committing|committed,
//!   result?}`.
//! - `handoff.write {id, offset}` → `{received}`. The data travels as a binary payload in the same
//!   encrypted message, after the JSON request and one NUL byte ([`split_payload`]); no base64 in
//!   the channel. `data_b64` is accepted as well (simple clients, tests). `offset` may be at most
//!   `received`: a lower offset truncates there and rewrites, so a retried chunk is harmless.
//! - `handoff.commit {id}` → the server's record `{incoming, state, result?}`, after checking size
//!   and sha256. Committing again returns the same record.
//! - `handoff.discard {id}`.
//!
//! Uploads live in memory and expire 24 h after their last write. A gateway restart forgets them
//! (the startup sweep removes their files) and the sender's next `handoff.status` gets
//! `not_found`, so it offers again from the start.

use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use vk_handoff::{MAX_BUNDLE, Manifest, clean, hash_file};

use crate::Gateway;
use crate::api::{ApiError, ApiResult};
use crate::handoff::{blocking, dir};
use crate::state::{Device, now_s};

/// Bytes a sender puts in one `handoff.write`.
pub const CHUNK: usize = 1024 * 1024;
/// The most one `handoff.write` may carry.
pub const MAX_WRITE: usize = 4 * 1024 * 1024;
/// An upload nobody wrote to for this long is dropped with its file.
pub const IDLE_TTL: Duration = Duration::from_secs(24 * 3600);
/// How long a committed upload's record is kept for repeated offers, status calls and commits.
const DONE_TTL: Duration = Duration::from_secs(24 * 3600);
/// Unfinished uploads per sending device, and in all.
const PER_DEVICE: usize = 2;
const TOTAL: usize = 8;

tokio::task_local! {
    /// The binary payload that came with the request being handled (see [`split_payload`]).
    pub static PAYLOAD: Option<Arc<Vec<u8>>>;
}

/// Split a decrypted request into its JSON text and the binary payload after the first NUL byte,
/// if any. JSON text never contains a raw NUL (control characters are always escaped), so the
/// first NUL ends the request.
pub fn split_payload(plain: &[u8]) -> (&[u8], Option<&[u8]>) {
    match plain.iter().position(|&b| b == 0) {
        Some(i) => (&plain[..i], Some(&plain[i + 1..])),
        None => (plain, None),
    }
}

/// Methods that accept a binary payload.
pub fn takes_payload(method: &str) -> bool {
    method == "handoff.write"
}

struct Upload {
    device: String,
    path: PathBuf,
    size: u64,
    sha256: String,
    manifest: Box<Manifest>,
    received: u64,
    last_write: Instant,
    /// A write or the commit runs outside the lock.
    busy: bool,
    committing: bool,
}

struct Done {
    device: String,
    sha256: String,
    size: u64,
    result: Value,
    at: Instant,
}

#[derive(Default)]
struct Uploads {
    live: HashMap<String, Upload>,
    done: HashMap<String, Done>,
}

static UPLOADS: Mutex<Option<Uploads>> = Mutex::new(None);

/// Runs `f` under the lock; files of expired uploads are removed after it is released.
fn with_uploads<T>(f: impl FnOnce(&mut Uploads) -> T) -> T {
    let mut expired = Vec::new();
    let r = {
        let mut g = UPLOADS.lock().unwrap();
        let u = g.get_or_insert_with(Uploads::default);
        u.live.retain(|_, e| {
            let keep = e.busy || e.last_write.elapsed() < IDLE_TTL;
            if !keep {
                expired.push(e.path.clone());
            }
            keep
        });
        u.done.retain(|_, d| d.at.elapsed() < DONE_TTL);
        f(u)
    };
    for p in expired {
        let _ = std::fs::remove_file(p);
    }
    r
}

/// Whether `id` names a peer upload, so `handoff.write` and `handoff.discard` come here rather
/// than to the courier flow (`handoff.rs`).
pub fn owns(id: &str) -> bool {
    with_uploads(|u| u.live.contains_key(id) || u.done.contains_key(id))
}

fn err(kind: &str, m: impl Into<String>) -> ApiError {
    ApiError::new(kind, m)
}

fn s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(|v| v.as_str())
}

fn req<'a>(p: &'a Value, k: &str) -> Result<&'a str, ApiError> {
    s(p, k)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::invalid(format!("{k} is required")))
}

fn no_such() -> ApiError {
    err("not_found", "no such upload")
}

/// Who sent a handoff, for the server's incoming record: `{host, owner: self|teammate, user?,
/// device}`. A peer device introduced itself at pairing (`pair.claim {peer}`); the owner's own
/// app counts as `self`; anything else is a teammate.
pub fn sender_of(dev: &Device) -> Value {
    let peer = dev.peer.as_ref();
    let owner = match dev.kind.as_str() {
        "device" => "self",
        "peer" if peer.is_some_and(|p| p.owner == "self") => "self",
        _ => "teammate",
    };
    let host = peer
        .and_then(|p| p.host_name.clone())
        .unwrap_or_else(|| dev.name.clone());
    let mut from = json!({"host": clean(&host, 80), "owner": owner, "device": dev.id});
    if let Some(user) = peer.and_then(|p| p.user.as_ref()) {
        from["user"] = json!(user);
    }
    from
}

/// Hand a received bundle to the server as an incoming handoff. The server keeps the bundle (it
/// moves or copies `path` into its own state) and runs the auto-import policy or leaves it
/// pending for the receiver. Returns the server's record `{incoming, state, result?}`. Shared by
/// the courier's `handoff.finish` and the peer's `handoff.commit`.
pub async fn deliver_to_server(
    gw: &Gateway,
    path: &Path,
    manifest: &Manifest,
    sha256: &str,
    from: &Value,
) -> ApiResult {
    let actor = format!("gateway:{}", s(from, "host").unwrap_or("peer"));
    gw.server
        .call_as(
            &actor,
            "handoff.incoming.add",
            json!({"path": path, "manifest": manifest, "sha256": sha256, "from": from}),
        )
        .await
}

/// Side effects re-check that the device is still authorized (spec 16 §4.6).
fn still_authorized(gw: &Gateway, dev: &Device) -> Result<(), ApiError> {
    let _ = gw.reload_devices();
    match gw.device(&dev.id) {
        Some(d) if !d.expired() => Ok(()),
        _ => Err(err("forbidden", "this device is no longer authorized")),
    }
}

pub async fn dispatch(gw: &Arc<Gateway>, dev: &Device, method: &str, p: &Value) -> ApiResult {
    match method {
        "handoff.offer" => offer(gw, dev, p),
        "handoff.status" => status(dev, p),
        "handoff.write" => write(dev, p).await,
        "handoff.commit" => commit(gw, dev, p).await,
        "handoff.discard" => discard(dev, p),
        _ => Err(err("method_not_found", method)),
    }
}

fn offer(gw: &Gateway, dev: &Device, p: &Value) -> ApiResult {
    let manifest: Manifest = serde_json::from_value(p.get("manifest").cloned().unwrap_or_default())
        .map_err(|e| ApiError::invalid(format!("manifest: {e}")))?;
    let size = p
        .get("size")
        .and_then(|v| v.as_u64())
        .filter(|n| *n > 0)
        .ok_or_else(|| ApiError::invalid("size"))?;
    let sha = s(p, "sha256")
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| ApiError::invalid("sha256"))?
        .to_ascii_lowercase();
    if size > MAX_BUNDLE {
        return Err(err("too_large", "handoff bundle exceeds 200 MiB"));
    }
    let folder = dir(gw)?;
    with_uploads(|u| -> ApiResult {
        if let Some((id, d)) = u
            .done
            .iter()
            .find(|(_, d)| d.device == dev.id && d.sha256 == sha)
        {
            return Ok(
                json!({"id": id, "received": d.size, "size": d.size, "committed": true, "result": d.result}),
            );
        }
        if let Some((id, e)) = u
            .live
            .iter()
            .find(|(_, e)| e.device == dev.id && e.sha256 == sha)
        {
            if e.size != size {
                return Err(err(
                    "conflict",
                    "this bundle was offered before with another size",
                ));
            }
            return Ok(json!({"id": id, "received": e.received, "size": e.size}));
        }
        // Per sender, so one host's abandoned transfers can't block everyone (plus a global cap).
        let mine = u.live.values().filter(|e| e.device == dev.id).count();
        if mine >= PER_DEVICE || u.live.len() >= TOTAL {
            return Err(err("rate_limited", "too many handoffs in progress"));
        }
        let id = ulid::Ulid::new().to_string().to_lowercase();
        let path = folder.join(format!("{id}.peer.tar.zst"));
        std::fs::File::create(&path).map_err(|e| err("internal", e.to_string()))?;
        u.live.insert(
            id.clone(),
            Upload {
                device: dev.id.clone(),
                path,
                size,
                sha256: sha,
                manifest: Box::new(manifest),
                received: 0,
                last_write: Instant::now(),
                busy: false,
                committing: false,
            },
        );
        Ok(json!({"id": id, "received": 0, "size": size}))
    })
}

fn status(dev: &Device, p: &Value) -> ApiResult {
    let id = req(p, "id")?;
    with_uploads(|u| -> ApiResult {
        if let Some(d) = u.done.get(id).filter(|d| d.device == dev.id) {
            return Ok(
                json!({"id": id, "received": d.size, "size": d.size, "state": "committed", "result": d.result}),
            );
        }
        let e = u
            .live
            .get(id)
            .filter(|e| e.device == dev.id)
            .ok_or_else(no_such)?;
        let state = if e.committing {
            "committing"
        } else {
            "receiving"
        };
        Ok(json!({"id": id, "received": e.received, "size": e.size, "state": state}))
    })
}

async fn write(dev: &Device, p: &Value) -> ApiResult {
    let id = req(p, "id")?.to_string();
    let offset = p
        .get("offset")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| ApiError::invalid("offset"))?;
    let data: Arc<Vec<u8>> = match PAYLOAD.try_with(|d| d.clone()).ok().flatten() {
        Some(d) => d,
        None => {
            let b64 = s(p, "data_b64")
                .ok_or_else(|| ApiError::invalid("data: a binary payload or data_b64"))?;
            Arc::new(B64.decode(b64).map_err(|_| ApiError::invalid("data_b64"))?)
        }
    };
    if data.len() > MAX_WRITE {
        return Err(err("too_large", "a write carries at most 4 MiB"));
    }
    let n = data.len() as u64;
    let path = with_uploads(|u| -> Result<PathBuf, ApiError> {
        let e = u
            .live
            .get_mut(&id)
            .filter(|e| e.device == dev.id)
            .ok_or_else(no_such)?;
        if e.busy {
            return Err(err(
                "conflict",
                "another write or the commit is in progress",
            ));
        }
        if offset > e.received {
            return Err(ApiError {
                kind: "conflict".into(),
                message: format!("expected offset {} or lower", e.received),
                details: json!({"received": e.received}),
            });
        }
        if offset + n > e.size {
            return Err(err("too_large", "more data than offered"));
        }
        e.busy = true;
        Ok(e.path.clone())
    })?;
    // Truncate to the offset first, so a rewritten or retried chunk simply replaces the tail.
    let wrote = blocking(move || {
        let mut f = std::fs::OpenOptions::new().write(true).open(&path)?;
        f.set_len(offset)?;
        f.seek(SeekFrom::Start(offset))?;
        f.write_all(&data)
    })
    .await;
    with_uploads(|u| -> ApiResult {
        let e = u.live.get_mut(&id).ok_or_else(no_such)?;
        e.busy = false;
        match wrote {
            Ok(()) => {
                e.received = offset + n;
                e.last_write = Instant::now();
                Ok(json!({"received": e.received}))
            }
            Err(x) => {
                // The tail may be partial; the next write at `offset` replaces it.
                e.received = offset;
                Err(x)
            }
        }
    })
}

async fn commit(gw: &Arc<Gateway>, dev: &Device, p: &Value) -> ApiResult {
    let id = req(p, "id")?.to_string();
    enum Next {
        Earlier(Value),
        Go(PathBuf, Box<Manifest>, String, u64),
    }
    let next = with_uploads(|u| -> Result<Next, ApiError> {
        if let Some(d) = u.done.get(&id).filter(|d| d.device == dev.id) {
            return Ok(Next::Earlier(d.result.clone()));
        }
        let e = u
            .live
            .get_mut(&id)
            .filter(|e| e.device == dev.id)
            .ok_or_else(no_such)?;
        if e.busy {
            return Err(err("conflict", "this upload is busy"));
        }
        if e.received != e.size {
            return Err(ApiError {
                kind: "conflict".into(),
                message: format!("received {} of {} bytes", e.received, e.size),
                details: json!({"received": e.received}),
            });
        }
        e.busy = true;
        e.committing = true;
        Ok(Next::Go(
            e.path.clone(),
            e.manifest.clone(),
            e.sha256.clone(),
            e.size,
        ))
    })?;
    let (path, manifest, sha, size) = match next {
        Next::Earlier(earlier) => return Ok(earlier),
        Next::Go(path, manifest, sha, size) => (path, manifest, sha, size),
    };
    let release = || {
        with_uploads(|u| {
            if let Some(e) = u.live.get_mut(&id) {
                e.busy = false;
                e.committing = false;
            }
        })
    };
    if let Err(e) = still_authorized(gw, dev) {
        release();
        return Err(e);
    }
    let p2 = path.clone();
    let hashed = blocking(move || hash_file(&p2)).await;
    match hashed {
        Ok((n, h)) if n == size && h == sha => {}
        Ok(_) => {
            // Corrupt or tampered: drop it; the sender offers again from the start.
            with_uploads(|u| u.live.remove(&id));
            let _ = std::fs::remove_file(&path);
            return Err(err(
                "conflict",
                "checksum mismatch; offer the handoff again",
            ));
        }
        Err(e) => {
            release();
            return Err(e);
        }
    }
    let from = sender_of(dev);
    match deliver_to_server(gw, &path, &manifest, &sha, &from).await {
        Ok(rec) => {
            with_uploads(|u| {
                u.live.remove(&id);
                u.done.insert(
                    id.clone(),
                    Done {
                        device: dev.id.clone(),
                        sha256: sha,
                        size,
                        result: rec.clone(),
                        at: Instant::now(),
                    },
                );
            });
            // The server keeps its own copy (or moved this one away).
            let _ = std::fs::remove_file(&path);
            gw.state.audit(
                &json!({"ts": now_s(), "event": "handoff.received", "device": dev.id,
                                   "from": from, "size": size, "incoming": rec.get("incoming")}),
            );
            Ok(rec)
        }
        Err(e) => {
            release();
            Err(e)
        }
    }
}

fn discard(dev: &Device, p: &Value) -> ApiResult {
    let id = s(p, "id").unwrap_or("");
    let gone = with_uploads(|u| -> Result<Option<Upload>, ApiError> {
        if u.done.get(id).is_some_and(|d| d.device == dev.id) {
            u.done.remove(id);
            return Ok(None);
        }
        let committing = u
            .live
            .get(id)
            .filter(|e| e.device == dev.id)
            .map(|e| e.committing);
        match committing {
            Some(true) => Err(err("conflict", "this upload is being committed")),
            Some(false) => Ok(u.live.remove(id)),
            None => Ok(None),
        }
    })?;
    if let Some(e) = gone {
        let _ = std::fs::remove_file(e.path);
    }
    Ok(json!({}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{GitUser, PeerInfo, Scope};

    fn device(kind: &str, owner: Option<&str>) -> Device {
        Device {
            id: format!("d-{kind}"),
            name: "the maintainer's laptop".into(),
            platform: "host".into(),
            public: "k".into(),
            scope: Scope::Full,
            paired_at: 0,
            vapid_private: None,
            push: vec![],
            prefs: Default::default(),
            push_failures: 0,
            kind: kind.into(),
            expires_at: None,
            limit: None,
            peer: owner.map(|o| PeerInfo {
                owner: o.into(),
                host_name: Some("marvin".into()),
                user: Some(GitUser {
                    name: Some("the maintainer".into()),
                    email: None,
                }),
            }),
        }
    }

    #[test]
    fn payload_split() {
        let mut msg = br#"{"method":"handoff.write","params":{"id":"x"}}"#.to_vec();
        assert_eq!(split_payload(&msg), (&msg[..], None));
        let json_len = msg.len();
        msg.push(0);
        msg.extend_from_slice(&[0, 1, 2, 0, 255]);
        let (body, payload) = split_payload(&msg);
        assert_eq!(body.len(), json_len);
        assert_eq!(payload, Some(&[0u8, 1, 2, 0, 255][..]));
        // A NUL inside a JSON string is always escaped, so it never splits the request.
        let escaped = serde_json::to_vec(&json!({"text": "a\u{0}b"})).unwrap();
        assert!(!escaped.contains(&0));
        assert!(takes_payload("handoff.write") && !takes_payload("handoff.commit"));
    }

    #[test]
    fn senders() {
        let own = sender_of(&device("peer", Some("self")));
        assert_eq!(own["owner"], "self");
        assert_eq!(own["host"], "marvin");
        assert_eq!(own["user"]["name"], "the maintainer");
        assert_eq!(
            sender_of(&device("peer", Some("teammate")))["owner"],
            "teammate"
        );
        assert_eq!(sender_of(&device("peer", None))["owner"], "teammate");
        assert_eq!(sender_of(&device("handoff", None))["owner"], "teammate");
        let app = sender_of(&device("device", None));
        assert_eq!(app["owner"], "self");
        assert_eq!(app["host"], "the maintainer's laptop");
        assert!(app.get("user").is_none());
    }
}
