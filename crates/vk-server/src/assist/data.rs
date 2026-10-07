//! Coordinator data kept in the session store's key-value table (scope `assistant`): capability
//! records, cached model lists, the result cache, remote source cursors and the background
//! planner's state. All are derived data: none holds a prompt, a credential or source text
//! (the cache holds validated drafts, bounded by retention and removed by `forget`).

use super::*;
use serde::de::DeserializeOwned;
use vk_assist::cache::{self, Cache};
use vk_assist::capability::{Capabilities, Feature, Records, Support};
use vk_assist::models::ModelList;
use vk_assist::remote::Cursors;

const KV_CAPS: &str = "capabilities";
const KV_CACHE: &str = "result_cache";
const KV_CURSORS: &str = "remote_cursors";
const KV_BG: &str = "background";

fn kv_key_models(connection: &str) -> String {
    format!("models:{connection}")
}

pub(super) fn read<T: DeserializeOwned + Default>(server: &Server, key: &str) -> T {
    server
        .with_core(|c| c.store.kv_get(KV_SCOPE, key).ok().flatten())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub(super) fn write<T: Serialize>(server: &Server, key: &str, v: &T) -> Result<(), RpcError> {
    let mut c = lk(&server.core);
    let mut tx = Tx::new();
    tx.m.kv(
        KV_SCOPE,
        key,
        Some(serde_json::to_string(v).unwrap_or_default()),
    );
    server.commit(&mut c, tx).map_err(crate::api::internal)?;
    Ok(())
}

// ---- capability records ------------------------------------------------------------------------

pub(super) fn records(server: &Server) -> Records {
    read(server, KV_CAPS)
}

/// What `r`'s model is believed to support: bundled, then observed/listed records, then the
/// profile's own declarations.
pub(super) fn capabilities(server: &Server, r: &Resolved) -> Capabilities {
    records(server).effective(r, now())
}

pub(super) fn observe(server: &Server, r: &Resolved, f: Feature, support: Support, note: &str) {
    let _g = lk(&state(server).kv);
    let mut rec = records(server);
    rec.observe(r, f, support, note, now());
    let _ = write(server, KV_CAPS, &rec);
}

pub(super) fn apply_listing(server: &Server, r: &Resolved, list: &ModelList) {
    let _g = lk(&state(server).kv);
    let mut rec = records(server);
    for m in &list.models {
        for (f, s) in &m.hints {
            rec.from_listing_for(r, &m.id, *f, *s, now());
        }
    }
    let _ = write(server, KV_CAPS, &rec);
}

// ---- model lists -------------------------------------------------------------------------------

/// The cached list for a connection, only while the connection's adapter/endpoint fingerprint
/// is the one it was fetched from.
pub(super) fn cached_models(
    server: &Server,
    connection: &str,
    fingerprint: &str,
) -> Option<ModelList> {
    let v: Value = server
        .with_core(|c| {
            c.store
                .kv_get(KV_SCOPE, &kv_key_models(connection))
                .ok()
                .flatten()
        })
        .and_then(|s| serde_json::from_str(&s).ok())?;
    if v["fingerprint"] != fingerprint {
        return None;
    }
    serde_json::from_value(v["list"].clone()).ok()
}

pub(super) fn store_models(server: &Server, fingerprint: &str, list: &ModelList) {
    let _ = write(
        server,
        &kv_key_models(&list.connection),
        &json!({"fingerprint": fingerprint, "list": list}),
    );
}

// ---- result cache ------------------------------------------------------------------------------

fn load_cache(server: &Server) -> Cache {
    read(server, KV_CACHE)
}

pub(super) fn cache_get(server: &Server, key: &str) -> Option<cache::Entry> {
    let _g = lk(&state(server).kv);
    load_cache(server).get(key, now()).cloned()
}

#[cfg(test)]
pub(super) fn cache_put(server: &Server, e: cache::Entry) {
    let _g = lk(&state(server).kv);
    put_locked(server, e);
}

fn put_locked(server: &Server, e: cache::Entry) {
    let mut c = load_cache(server);
    c.sweep(now());
    c.put(e);
    let _ = write(server, KV_CACHE, &c);
}

/// Insert a result unless a purge ran after generation `gen` (checked and inserted under the
/// same lock purges take): a purge or forget that finished before the insert stays effective.
/// Returns whether it was inserted.
pub(super) fn cache_put_unless_purged(server: &Server, gen_at_done: u64, e: cache::Entry) -> bool {
    let st = state(server);
    let _g = lk(&st.kv);
    if st.cache_gen.load(std::sync::atomic::Ordering::SeqCst) != gen_at_done {
        return false;
    }
    put_locked(server, e);
    true
}

pub(super) fn cache_len(server: &Server) -> usize {
    load_cache(server).entries.len()
}

pub(super) enum CacheScope<'a> {
    All,
    Workspace(&'a str),
    /// Entries derived from one request.
    Origin(&'a str),
    /// Entries created before this time (`forget --before`).
    Before(i64),
}

/// Remove derived results from the cache (`forget`/`purge`). Returns how many went.
pub(super) fn cache_purge(server: &Server, scope: CacheScope<'_>) -> usize {
    let st = state(server);
    let _g = lk(&st.kv);
    // Results completed before this purge must not be inserted after it.
    st.cache_gen
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut c = load_cache(server);
    let n = match scope {
        CacheScope::All => c.purge_all(),
        CacheScope::Workspace(w) => c.purge_workspace(w),
        CacheScope::Before(ms) => c.purge_before(ms),
        CacheScope::Origin(o) => {
            let before = c.entries.len();
            c.entries.retain(|e| e.data["origin"] != o);
            before - c.entries.len()
        }
    };
    if n > 0 {
        let _ = write(server, KV_CACHE, &c);
    }
    n
}

pub(super) fn cache_sweep(server: &Server) {
    let _g = lk(&state(server).kv);
    let mut c = load_cache(server);
    if c.sweep(now()) > 0 {
        let _ = write(server, KV_CACHE, &c);
    }
}

// ---- remote cursors ------------------------------------------------------------------------------

pub(super) fn cursors(server: &Server) -> Cursors {
    read(server, KV_CURSORS)
}

pub(super) fn store_cursors(server: &Server, c: &Cursors) {
    let _g = lk(&state(server).kv);
    let _ = write(server, KV_CURSORS, c);
}

// ---- background planner state ----------------------------------------------------------------------

pub(super) fn bg_state(server: &Server) -> vk_assist::background::BgState {
    read(server, KV_BG)
}

pub(super) fn store_bg_state(server: &Server, s: &vk_assist::background::BgState) {
    let _g = lk(&state(server).kv);
    let _ = write(server, KV_BG, s);
}
