//! `state.forget {pane | workspace | before | all, dry_run?, plan?, scrollback_only?}` (09 §9.3,
//! lane 3E): `vibeke forget` for everything Vibeke stored about a scope, not only scrollback.
//!
//! Order: the scrollback archive first (`scrollback.forget`, which resolves the scope, checks a
//! confirmed `plan` and refuses before anything is deleted), then screenshots and other session
//! blobs, pane inbox uploads, drafts and workspace notes, the session desk
//! index, VT snapshots of panes that are no longer live, and last the event log (events in scope
//! become tombstones so `seq` stays gapless). Spec-15 derived objects (lane 2C,
//! `review::purge`) and assistant records (lane 2D, `assist::forget_scope`) are purged inside
//! `scrollback.forget` itself and reported under `review` / `also.assistant`, not repeated here;
//! the Turn/Item stream and the unified blob store (lane 3D) are purged here. One `state.forgotten` event records the scope and
//! counts, never content.
//!
//! What each scope reaches:
//! - `pane`: everything tied to the pane id (blob/screenshot `pane`, inbox uploads made from
//!   that pane, the desk sessions of runs in the pane, its snapshot when closed, events whose
//!   subject names the pane). Drafts and notes are per workspace, so a pane scope leaves them
//!   (reported in `not_covered`).
//! - `workspace`: the above for every pane of the workspace plus objects recorded with the
//!   workspace (drafts, notes, desk rows, events naming it).
//! - `before`: objects created (events recorded) before the time; drafts and notes by last
//!   update.
//! - `all`: everything above.
//!
//! Not covered: the live screen and in-memory scrollback of running panes, the snapshot a live
//! pane recovers from, task and review records themselves (their derived content is purged by lane 2C), the
//! audit log (append-only by design), native harness transcripts (owned by the harness).

use crate::Server;
use crate::api::{Ctx, R, b, internal};
use crate::core::Tx;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use vk_store::EventScope;

/// The resolved scope of a forget.
#[derive(Debug, Clone, PartialEq)]
pub enum Scope {
    All,
    Panes(Vec<String>),
    Workspace { id: String, panes: Vec<String> },
    Before(i64),
}

impl Scope {
    /// From `scrollback.forget`'s canonical result (`scope`, `pane_ids`).
    pub fn from_plan(plan: &Value) -> Option<Scope> {
        let sc = &plan["scope"];
        let ids: Vec<String> = plan["pane_ids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if sc["all"] == json!(true) {
            Some(Scope::All)
        } else if sc.get("pane").is_some() {
            Some(Scope::Panes(ids))
        } else if let Some(w) = sc["workspace"].as_str() {
            Some(Scope::Workspace {
                id: w.to_string(),
                panes: ids,
            })
        } else {
            sc["before"].as_i64().map(Scope::Before)
        }
    }

    fn panes(&self) -> &[String] {
        match self {
            Scope::Panes(p) | Scope::Workspace { panes: p, .. } => p,
            _ => &[],
        }
    }

    /// Does an object owned by `pane` / `workspace`, created at `at_ms`, fall in scope?
    pub fn covers(&self, pane: Option<&str>, workspace: Option<&str>, at_ms: Option<i64>) -> bool {
        let in_panes = |p: Option<&str>| p.is_some_and(|p| self.panes().iter().any(|x| x == p));
        match self {
            Scope::All => true,
            Scope::Panes(_) => in_panes(pane),
            Scope::Workspace { id, .. } => in_panes(pane) || workspace == Some(id.as_str()),
            Scope::Before(t) => at_ms.is_some_and(|a| a < *t),
        }
    }

    fn event_scope(&self) -> EventScope {
        match self {
            Scope::All => EventScope::All,
            Scope::Panes(p) => EventScope::Panes(p.clone()),
            Scope::Workspace { id, panes } => EventScope::Workspace {
                id: id.clone(),
                panes: panes.clone(),
            },
            Scope::Before(t) => EventScope::Before(*t),
        }
    }
}

pub async fn forget(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let dry = b(p, "dry_run").unwrap_or(false);
    let mut sp = p.clone();
    let scrollback_only = sp
        .as_object_mut()
        .and_then(|o| o.remove("scrollback_only"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Resolves the scope and refuses a stale confirmed plan before anything is deleted.
    let mut out = crate::search::forget(server, ctx, &sp)?;
    if scrollback_only {
        out["scrollback_only"] = json!(true);
        return Ok(out);
    }
    let scope =
        Scope::from_plan(&out).ok_or_else(|| internal("scrollback.forget returned no scope"))?;
    let user = crate::drafts::user_ctx();
    let mut also = serde_json::Map::new();
    // Turn/Item stream first (3D): payload blobs that remaining items still reference stay.
    also.insert("items".into(), json!(forget_items(server, &scope, dry)));
    // Collision records and claims (05 §10, 3A) name the runs and paths that were edited.
    also.insert(
        "collisions".into(),
        json!(forget_collisions(server, &scope, dry)),
    );
    also.insert("blobs".into(), json!(forget_blobs(server, &scope, dry)));
    also.insert("uploads".into(), json!(forget_uploads(server, &scope, dry)));
    also.insert(
        "drafts".into(),
        json!(forget_drafts(server, &user, &scope, dry).await),
    );
    also.insert("assistant".into(), assistant_report(&out, dry));
    also.insert("desk".into(), forget_desk(server, &user, &scope, dry).await);
    let live: Vec<String> = server.panes.lock().unwrap().keys().cloned().collect();
    let (snapshots, events) = server.with_core(|c| {
        let snaps = match &scope {
            Scope::All => c.store.forget_snapshots(None, None, &live, dry),
            Scope::Before(t) => c.store.forget_snapshots(None, Some(*t), &live, dry),
            s => c.store.forget_snapshots(Some(s.panes()), None, &live, dry),
        };
        let ev = c.store.tombstone_events(&scope.event_scope(), dry);
        (snaps, ev)
    });
    also.insert("snapshots".into(), json!(snapshots.map_err(internal)?));
    also.insert("events_tombstoned".into(), json!(events.map_err(internal)?));
    let also = Value::Object(also);
    if !dry {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        // Metadata only: scope and counts.
        tx.event(
            "state.forgotten",
            json!({"scope": out["scope"]}),
            json!({"counts": counts_only(&also)}),
        );
        server.commit(&mut c, tx).map_err(internal)?;
    }
    let mut not_covered = vec![
        "live screens and in-memory scrollback of running panes",
        "VT snapshots of live panes",
        "task and review records (their spec-15 derived content is purged; see `review`)",
        "the audit log",
        "native harness transcripts",
    ];
    if matches!(scope, Scope::Panes(_)) {
        not_covered.push("drafts and workspace notes (per workspace)");
    }
    out["also"] = also;
    out["not_covered"] = json!(not_covered);
    Ok(out)
}

/// `{"blobs": {"removed": 2, ...}, ...}` → just the numbers, for the event.
fn counts_only(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(_, v)| v.is_number() || v.is_object())
                .map(|(k, v)| (k.clone(), counts_only(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn time_of(v: &Value) -> Option<i64> {
    ["created_at_ms", "at_ms", "taken_at", "created_at"]
        .iter()
        .find_map(|k| v.get(*k).and_then(Value::as_i64))
}

/// Turns and items (02 §1.1 Turn/Item stream, 3D) of runs in scope, or started before the cutoff.
fn forget_items(server: &Server, scope: &Scope, dry: bool) -> usize {
    let runs: HashSet<String> = match scope {
        Scope::All | Scope::Before(_) => HashSet::new(),
        s => runs_of(server, s.panes()).into_iter().collect(),
    };
    let covers = |run: &str, at: i64| match scope {
        Scope::All => true,
        Scope::Before(t) => at < *t,
        _ => runs.contains(run),
    };
    crate::items::forget(server, &covers, dry)
}

/// Collision records and claims (05 §10, 3A) of runs in scope, or created before the cutoff.
fn forget_collisions(server: &Server, scope: &Scope, dry: bool) -> usize {
    let runs: HashSet<String> = match scope {
        Scope::All | Scope::Before(_) => HashSet::new(),
        s => runs_of(server, s.panes()).into_iter().collect(),
    };
    let covers = |run: &str, at: i64| match scope {
        Scope::All => true,
        Scope::Before(t) => at < *t,
        _ => runs.contains(run),
    };
    crate::collision::forget(server, &covers, dry)
}

/// Ids of runs (live and recently closed) in these panes.
fn runs_of(server: &Server, panes: &[String]) -> Vec<String> {
    use vk_proto::model::AgentRun;
    server.with_core(|c| {
        let mut runs: Vec<AgentRun> = c.model.runs.clone();
        runs.extend(
            c.store
                .load_closed::<AgentRun>("run", 2000)
                .unwrap_or_default(),
        );
        runs.into_iter()
            .filter(|r| panes.contains(&r.pane))
            .map(|r| r.id)
            .collect()
    })
}

/// Screenshot records and session blob-store files in scope.
fn forget_blobs(server: &Server, scope: &Scope, dry: bool) -> Value {
    use crate::screenshots::{ScreenshotMeta, load_all, remove_records};
    let records = load_all(server);
    let gone: Vec<ScreenshotMeta> = records
        .iter()
        .filter(|m| {
            scope.covers(
                m.pane.as_deref(),
                m.workspace.as_deref(),
                Some(m.created_at_ms),
            )
        })
        .cloned()
        .collect();
    let gone_ids: HashSet<&str> = gone.iter().map(|m| m.id.as_str()).collect();
    // Blobs a remaining record or Turn/Item payload still points at stay.
    let mut kept_blobs: HashSet<String> = records
        .iter()
        .filter(|m| !gone_ids.contains(m.id.as_str()))
        .map(|m| m.blob.clone())
        .collect();
    kept_blobs.extend(crate::items::payload_refs(server));
    let screenshots = gone.len();
    let mut removed = if dry {
        gone.iter()
            .map(|m| &m.blob)
            .collect::<HashSet<_>>()
            .iter()
            .filter(|b| !kept_blobs.contains(b.as_str()))
            .count()
    } else {
        remove_records(server, &gone, "forgotten")
    };
    let gone_blobs: HashSet<String> = gone.iter().map(|m| m.blob.clone()).collect();
    // Other blobs of the unified store (pane screenshots, diffs, item payloads) by their sidecar
    // metadata. Ingested uploads (`source: inbox`) go with their upload in `forget_uploads`,
    // which knows every recorded owner.
    let root = server.paths.blobs();
    for d in std::fs::read_dir(&root).into_iter().flatten().flatten() {
        for e in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let Some(hash) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else {
                continue;
            };
            if kept_blobs.contains(&hash) || gone_blobs.contains(&hash) {
                continue;
            }
            let Ok(meta) = std::fs::read(&p).map(|b| serde_json::from_slice::<Value>(&b)) else {
                continue;
            };
            let Ok(meta) = meta else { continue };
            if str_of(&meta, "source") == Some("inbox") {
                continue;
            }
            let at = time_of(&meta).or_else(|| mtime_ms(&p));
            if !scope.covers(str_of(&meta, "pane"), str_of(&meta, "workspace"), at) {
                continue;
            }
            removed += 1;
            if dry {
                continue;
            }
            for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                let n = f.file_name().to_string_lossy().into_owned();
                if n.starts_with(&format!("{hash}.")) {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
        if !dry {
            let _ = std::fs::remove_dir(d.path());
        }
    }
    json!({"screenshots": screenshots, "files": removed})
}

/// Blob hashes something that survives the forget still points at: remaining screenshot
/// records and Turn/Item payloads (items and screenshots in scope are already gone when the
/// uploads are forgotten).
fn surviving_blob_refs(server: &Server) -> HashSet<String> {
    let mut v: HashSet<String> = crate::screenshots::load_all(server)
        .into_iter()
        .map(|m| m.blob)
        .collect();
    v.extend(crate::items::payload_refs(server));
    v
}

fn mtime_ms(p: &Path) -> Option<i64> {
    std::fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

/// Pane inbox uploads this session recorded (`blob_owner`) whose every owner is in scope.
/// The inbox is shared by the installation's sessions, so a file another owner still claims
/// stays.
fn forget_uploads(server: &Server, scope: &Scope, dry: bool) -> Value {
    let rows = server.with_core(|c| c.store.kv_scope("blob_owner").unwrap_or_default());
    let inbox = crate::paths::Paths::inbox();
    let surviving = surviving_blob_refs(server);
    let (mut removed, mut kept) = (0u64, 0u64);
    let mut drop_keys: Vec<String> = vec![];
    for (hash, owners) in rows {
        if hash.len() < 12 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
            continue;
        }
        let owners: Vec<Value> = serde_json::from_str(&owners).unwrap_or_default();
        let dir = inbox.join(&hash[..12]);
        let at = mtime_ms(&dir);
        let all_in = !owners.is_empty()
            && owners
                .iter()
                .all(|o| scope.covers(str_of(o, "pane"), str_of(o, "workspace"), at));
        if !all_in {
            if owners
                .iter()
                .any(|o| scope.covers(str_of(o, "pane"), str_of(o, "workspace"), at))
            {
                kept += 1;
            }
            continue;
        }
        removed += 1;
        drop_keys.push(hash.clone());
        if dry {
            continue;
        }
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = e.path();
            let same = std::fs::File::open(&p).ok().and_then(|mut f| {
                let mut h = blake3::Hasher::new();
                std::io::copy(&mut f, &mut h).ok()?;
                Some(h.finalize().to_hex().to_string() == hash)
            });
            if same == Some(true) {
                let _ = std::fs::remove_file(&p);
            }
        }
        let _ = std::fs::remove_dir(&dir);
        // Its copy in the unified blob store (3D) goes too — unless a surviving screenshot
        // record or Turn/Item payload has the same content (the same protection as
        // `forget_blobs`): another pane's retained payload must stay readable.
        if !surviving.contains(&hash) {
            crate::blob_store::store(server).remove(&hash);
        }
    }
    if !dry && !drop_keys.is_empty() {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        for k in &drop_keys {
            tx.m.kv("blob_owner", k, None);
        }
        let _ = server.commit(&mut c, tx);
    }
    json!({"removed": removed, "kept_shared": kept})
}

async fn forget_drafts(server: &Arc<Server>, user: &Ctx, scope: &Scope, dry: bool) -> Value {
    use crate::drafts::{Draft, K_DRAFT, K_NOTES, Notes};
    if matches!(scope, Scope::Panes(_)) {
        return json!({"drafts": 0, "notes": 0, "failed": 0});
    }
    let (drafts, notes) = server.with_core(|c| {
        (
            c.store.load::<Draft>(K_DRAFT).unwrap_or_default(),
            c.store.load::<Notes>(K_NOTES).unwrap_or_default(),
        )
    });
    let drafts: Vec<Draft> = drafts
        .into_iter()
        .filter(|d| scope.covers(None, d.workspace.as_deref(), Some(d.updated_at_ms)))
        .collect();
    let notes: Vec<Notes> = notes
        .into_iter()
        .filter(|n| scope.covers(None, Some(&n.workspace), Some(n.updated_at_ms)))
        .collect();
    if dry {
        return json!({"drafts": drafts.len(), "notes": notes.len(), "failed": 0});
    }
    let (mut deleted, mut failed) = (0u64, 0u64);
    for d in &drafts {
        // Through `draft.delete`: a draft being sent is refused (kept), events stay metadata.
        match crate::drafts::api(server, user, "draft.delete", &json!({"draft": d.id})).await {
            Some(Ok(_)) => deleted += 1,
            _ => failed += 1,
        }
    }
    if !notes.is_empty() {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        for n in &notes {
            tx.m.delete(K_NOTES, &n.workspace);
            tx.event(
                "notes.updated",
                json!({"workspace": n.workspace}),
                json!({"rev": n.rev + 1, "bytes": 0}),
            );
        }
        let _ = server.commit(&mut c, tx);
    }
    json!({"drafts": deleted, "notes": notes.len(), "failed": failed})
}

/// Assistant records are purged by `scrollback.forget` itself (lane 2D, `assist::forget_scope`,
/// every scope); this only reports its count (`assistant_purged`), never purges again.
fn assistant_report(out: &Value, dry: bool) -> Value {
    if dry {
        json!({"purged": Value::Null, "by": "scrollback.forget"})
    } else {
        json!({"purged": out["assistant_purged"].as_u64().unwrap_or(0), "by": "scrollback.forget"})
    }
}

async fn forget_desk(server: &Arc<Server>, user: &Ctx, scope: &Scope, dry: bool) -> Value {
    let calls: Vec<Value> = match scope {
        Scope::All => vec![json!({"before": vk_store::now_ms() + 86_400_000})],
        Scope::Before(t) => vec![json!({"before": t})],
        Scope::Workspace { id, panes } => {
            let mut v = vec![json!({"workspace": id})];
            v.extend(
                sessions_of(server, panes)
                    .into_iter()
                    .map(|s| json!({"session": s})),
            );
            v
        }
        Scope::Panes(panes) => sessions_of(server, panes)
            .into_iter()
            .map(|s| json!({"session": s}))
            .collect(),
    };
    if dry {
        // Count the rows the calls would delete, so a scope with nothing left reads as empty
        // (`vibeke forget` then says "nothing to forget" instead of asking to delete nothing).
        let (mut sessions, mut workspaces, mut before) = (vec![], vec![], None);
        for c in &calls {
            if let Some(x) = c["session"].as_str() {
                sessions.push(x.to_string());
            }
            if let Some(x) = c["workspace"].as_str() {
                workspaces.push(x.to_string());
            }
            if let Some(x) = c["before"].as_i64() {
                before = Some(x);
            }
        }
        let rows = crate::desk::forget_count(server, sessions, workspaces, before)
            .await
            .ok();
        return json!({"calls": calls.len(), "rows": rows});
    }
    let mut rows = 0u64;
    let mut errors = vec![];
    for p in &calls {
        match crate::desk::api(server, user, "desk.forget", p).await {
            Some(Ok(v)) => rows += v["rows_deleted"].as_u64().unwrap_or(0),
            Some(Err(e)) => errors.push(e.message),
            None => {}
        }
    }
    json!({"calls": calls.len(), "rows": rows, "errors": errors})
}

/// Native conversation ids of runs (live and recently closed) in these panes.
fn sessions_of(server: &Server, panes: &[String]) -> Vec<String> {
    use vk_proto::model::AgentRun;
    let mut out: Vec<String> = server.with_core(|c| {
        let mut runs: Vec<AgentRun> = c.model.runs.clone();
        runs.extend(
            c.store
                .load_closed::<AgentRun>("run", 2000)
                .unwrap_or_default(),
        );
        runs.into_iter()
            .filter(|r| panes.contains(&r.pane))
            .filter_map(|r| r.harness_session_id)
            .collect()
    });
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_from_plan_and_coverage() {
        let s = Scope::from_plan(&json!({"scope": {"pane": "p1"}, "pane_ids": ["p1"]})).unwrap();
        assert!(s.covers(Some("p1"), None, None));
        assert!(!s.covers(Some("p2"), Some("w1"), None));
        let w = Scope::from_plan(&json!({"scope": {"workspace": "w1"}, "pane_ids": ["p1", "p2"]}))
            .unwrap();
        assert!(w.covers(None, Some("w1"), None));
        assert!(w.covers(Some("p2"), None, None));
        assert!(!w.covers(Some("p9"), Some("w2"), None));
        let b = Scope::from_plan(&json!({"scope": {"before": 100}, "pane_ids": null})).unwrap();
        assert!(b.covers(None, None, Some(99)));
        assert!(!b.covers(Some("p1"), None, Some(100)));
        assert!(!b.covers(None, None, None));
        let a = Scope::from_plan(&json!({"scope": {"all": true}, "pane_ids": null})).unwrap();
        assert!(a.covers(None, None, None));
        assert!(Scope::from_plan(&json!({"scope": {}})).is_none());
    }

    #[test]
    fn counts_only_drops_text() {
        let v = json!({"desk": {"rows": 3, "errors": ["x"]}, "events_tombstoned": 4});
        assert_eq!(
            counts_only(&v),
            json!({"desk": {"rows": 3}, "events_tombstoned": 4})
        );
    }
}
