//! The unified blob store as the server uses it (02 §1.1 "Blob"; 3D): every blob lives in the
//! session store `<state>/blobs/<h2>/<blake3>.<ext>` (`vk_store::blobs`), whatever made it.
//!
//! - Screenshots and pane screenshots already write there (`source: screenshot`).
//! - Uploaded files (`blob.put`, chunked `blob.commit`) keep their path in the pane inbox, because
//!   the agent needs a path, and are ingested here too (`source: inbox`), so `blob.get/stat` and
//!   `blob.gc` see one store. [`adopt_legacy`] ingests uploads made before this existed.
//! - Tool outputs, large diffs and long messages of the Turn/Item stream (`items`) are stored
//!   with [`put_payload`] (`source: payload`) and referenced by `Item.payload_ref`.
//!
//! `blob.stats` reports the store by source; `blob.gc` removes unreferenced, old blobs of the
//! two collectable sources (`inbox`, `payload`) and nothing else (screenshots follow their own
//! retention in `screenshots.rs`).

use crate::Server;
use crate::api::{Ctx, R, b, invalid};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;
use vk_proto::model::Pane;
use vk_store::blobs::{BlobStore, MetaMode};

pub const METHODS: &[(&str, bool)] = &[("blob.stats", false), ("blob.gc", true)];

/// Refused to pane tokens: the store spans every workspace of the session.
pub const PANE_FORBIDDEN: &[&str] = &["blob.stats", "blob.gc"];

/// Default `blob.gc` age (and the age of the hourly sweep): payloads and uploads younger than
/// this are never collected.
pub const DEFAULT_GC_DAYS: i64 = 30;

/// The session store; blobs written through it are sealed while `security.encrypt_state` is
/// active (09 §9.1, `privacy::cipher`).
pub fn store(server: &Server) -> BlobStore {
    BlobStore::new(server.paths.blobs()).with_cipher(crate::privacy::cipher(server))
}

fn ext_of(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|e| !e.is_empty() && e.len() <= 8 && e.bytes().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "bin".into())
}

fn pane_ws(server: &Server, pane: &str) -> Option<String> {
    server.with_core(|c| c.pane(pane).map(|p: &Pane| p.workspace.clone()))
}

/// Record an uploaded inbox file in the blob store. Best effort: the upload already succeeded
/// and its inbox path is what the caller gets, so a failure here only leaves the blob readable
/// from the inbox (`blob_api::find` falls back to it).
pub fn ingest_upload(server: &Server, ctx: &Ctx, path: &Path, mime: Option<&str>) {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("upload")
        .to_string();
    let workspace = ctx.pane_scope.as_deref().and_then(|p| pane_ws(server, p));
    let mime = mime
        .map(str::to_string)
        .unwrap_or_else(|| crate::blob_api::mime_for(path).to_string());
    let meta = json!({
        "source": "inbox",
        "name": name,
        "mime": mime,
        "pane": ctx.pane_scope,
        "workspace": workspace,
        "created_at_ms": vk_store::now_ms(),
    });
    if let Err(e) = store(server).put_file(path, &ext_of(&name), &meta, MetaMode::IfAbsent) {
        tracing::debug!(error = %e, path = %path.display(), "blob ingest skipped");
    }
}

/// Dispatch hook after a successful `blob.put` / `blob.commit`: ingest the inbox file the result
/// names. Browser-staged drops and unpacked directories are not blobs and are skipped.
pub fn ingest_result(server: &Server, ctx: &Ctx, params: &Value, result: &Value) {
    if params.get("stage").and_then(Value::as_str) == Some("browser")
        || params.get("unpack").and_then(Value::as_str) == Some("tar")
        || result.get("unpacked").and_then(Value::as_bool) == Some(true)
    {
        return;
    }
    let Some(path) = result.get("path_on_machine").and_then(Value::as_str) else {
        return;
    };
    let path = Path::new(path);
    if !path.starts_with(crate::paths::Paths::inbox()) || !path.is_file() {
        return;
    }
    let mime = params.get("mime").and_then(Value::as_str);
    ingest_upload(server, ctx, path, mime);
}

/// Store a payload (tool output, diff, long message) for an item. Readable by the owning pane's
/// workspace. Returns the blake3 hash.
pub fn put_payload(
    server: &Server,
    data: &[u8],
    ext: &str,
    mime: &str,
    pane: Option<&str>,
) -> std::io::Result<String> {
    let workspace = pane.and_then(|p| pane_ws(server, p));
    let meta = json!({
        "source": "payload",
        "mime": mime,
        "pane": pane,
        "workspace": workspace,
        "created_at_ms": vk_store::now_ms(),
    });
    store(server)
        .put(data, ext, &meta, MetaMode::IfAbsent)
        .map(|(h, _)| h)
}

/// Ingest the uploads this session recorded (`blob_owner`) before the stores were unified. The
/// inbox is installation-wide, so only hashes this session owns are touched. Returns how many
/// blobs were added.
pub fn adopt_legacy(server: &Server) -> usize {
    let owned: Vec<String> = server.with_core(|c| {
        c.store
            .kv_scope("blob_owner")
            .unwrap_or_default()
            .into_iter()
            .map(|(k, _)| k)
            .collect()
    });
    let bs = store(server);
    let mut n = 0;
    for hash in owned {
        if hash.len() < 12 || bs.find(&hash).is_some() {
            continue;
        }
        let dir = crate::paths::Paths::inbox().join(&hash[..12]);
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if !e.metadata().is_ok_and(|m| m.is_file()) {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            let owners: Vec<Value> = server.with_core(|c| {
                c.store
                    .kv_get("blob_owner", &hash)
                    .ok()
                    .flatten()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default()
            });
            let first = owners.first().cloned().unwrap_or(Value::Null);
            let meta = json!({
                "source": "inbox",
                "name": name,
                "mime": crate::blob_api::mime_for(&p),
                "pane": first["pane"],
                "workspace": first["workspace"],
                "created_at_ms": vk_store::now_ms(),
                "adopted": true,
            });
            // Only a file that really hashes to the owned hash counts.
            if vk_store::blobs::hash_file(&p).ok().as_deref() == Some(hash.as_str())
                && bs
                    .put_file(&p, &ext_of(&name), &meta, MetaMode::IfAbsent)
                    .is_ok()
            {
                n += 1;
            }
        }
    }
    n
}

/// Hashes something still points at: item payloads of the Turn/Item stream.
pub fn referenced(server: &Server) -> HashSet<String> {
    crate::items::payload_refs(server)
}

pub fn gc(server: &Server, older_than_days: i64, dry_run: bool) -> vk_store::blobs::GcReport {
    let refs = referenced(server);
    store(server).gc(
        &refs,
        vk_store::now_ms(),
        older_than_days.max(0) * 86_400_000,
        dry_run,
    )
}

fn stats(server: &Server) -> R {
    let st = store(server).stats();
    let by_source: serde_json::Map<String, Value> = st
        .by_source
        .iter()
        .map(|(k, (n, bytes))| {
            (
                if k.is_empty() {
                    "unknown".into()
                } else {
                    k.clone()
                },
                json!({"count": n, "bytes": bytes}),
            )
        })
        .collect();
    Ok(
        json!({"count": st.count, "bytes": st.bytes, "by_source": by_source, "path": server.paths.blobs()}),
    )
}

fn gc_api(server: &Server, p: &Value) -> R {
    let days = match p.get("older_than_days") {
        None | Some(Value::Null) => DEFAULT_GC_DAYS,
        Some(v) => v
            .as_i64()
            .filter(|d| *d >= 0)
            .ok_or_else(|| invalid("older_than_days must be a non-negative integer"))?,
    };
    let dry = b(p, "dry_run").unwrap_or(false);
    let r = gc(server, days, dry);
    Ok(json!({
        "dry_run": dry,
        "older_than_days": days,
        "removed": r.removed,
        "bytes": r.bytes,
        "kept_referenced": r.kept_referenced,
        "kept_young": r.kept_young,
        "kept_uncollectable": r.kept_uncollectable,
        "hashes": r.hashes,
    }))
}

/// Dispatch hook for `blob.stats` / `blob.gc`.
pub fn api(server: &Server, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "blob.stats" => stats(server),
        "blob.gc" => gc_api(server, p),
        _ => return None,
    })
}

#[cfg(test)]
#[path = "blob_store_tests.rs"]
mod tests;
