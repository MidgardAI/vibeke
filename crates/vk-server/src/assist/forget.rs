//! `forget` integration (14 §8): forgetting a source scope also removes the assistant's
//! derived results and cached excerpts for it.
//!
//! `scrollback.forget` and `desk.forget` call [`forget_scope`] with the canonical scope they
//! resolved (`all`, `pane`, `workspace`, `repo`, `before`). Matching assistant request
//! records (and their outputs) are deleted, unfinished ones are cancelled first, and the
//! result cache entries derived from the workspace are dropped. Only metadata is logged.
//! Scopes that name neither a workspace, pane, repository, time nor "all" (a bare desk
//! session id) cannot be attributed to assistant records and leave them alone.

use super::*;

/// Remove assistant records and cache entries derived from `scope`. Returns how many request
/// records were deleted.
pub fn forget_scope(server: &Server, scope: &Value) -> usize {
    let all = scope["all"] == true;
    let workspace = scope["workspace"].as_str();
    let pane = scope["pane"].as_str();
    let repo = scope["repo"].as_str();
    let before = scope["before"].as_i64();
    if !all && workspace.is_none() && pane.is_none() && repo.is_none() && before.is_none() {
        return 0;
    }
    // Workspaces (id and canonical path) the scope covers.
    let (ws_ids, ws_paths): (Vec<String>, Vec<String>) = server.with_core(|c| {
        let mut ids = vec![];
        let mut paths = vec![];
        for w in &c.model.workspaces {
            let canon = canonical(&w.root_path);
            let hit = workspace.is_some_and(|x| x == w.id || x == w.handle || x == canon)
                || repo.is_some_and(|r| {
                    let r = canonical(r);
                    canon == r || canon.starts_with(&format!("{r}/"))
                })
                || pane.is_some_and(|pid| c.pane(pid).is_some_and(|p| p.workspace == w.id));
            if hit {
                ids.push(w.id.clone());
                paths.push(canon);
            }
        }
        (ids, paths)
    });
    let victims: Vec<AssistRequest> = all_requests(server)
        .into_iter()
        .filter(|r| {
            if all {
                return true;
            }
            if before.is_some_and(|b| r.created_at_ms < b) {
                return true;
            }
            if ws_ids.contains(&r.workspace)
                || ws_paths.iter().any(|p| r.workspace_paths().any(|w| w == p))
            {
                // A pane scope only covers requests about that pane's content.
                return pane.is_none()
                    || r.inputs["pane"].as_str() == pane
                    || r.inputs["run"].as_str().is_some_and(|run| {
                        server.with_core(|c| {
                            c.run(run).is_some_and(|x| Some(x.pane.as_str()) == pane)
                        })
                    });
            }
            workspace.is_some_and(|w| r.workspace == w || r.workspace_path == w)
        })
        .collect();
    for r in &victims {
        if r.state.open() {
            let _ = cancel_one(server, r.clone(), Category::Cancelled, "forgotten");
        }
    }
    let mut cache = 0;
    if all {
        cache += data::cache_purge(server, data::CacheScope::All);
    }
    if let Some(b) = before {
        cache += data::cache_purge(server, data::CacheScope::Before(b));
    }
    for p in &ws_paths {
        cache += data::cache_purge(server, data::CacheScope::Workspace(p));
    }
    for r in &victims {
        cache += data::cache_purge(server, data::CacheScope::Origin(&r.id));
    }
    if victims.is_empty() && cache == 0 {
        return 0;
    }
    let mut c = lk(&server.core);
    let mut tx = Tx::new();
    for r in &victims {
        tx.m.delete(K_REQ, &r.id);
    }
    tx.event(
        "assistant.purged",
        json!({}),
        json!({"count": victims.len(), "cache_entries": cache, "reason": "forget"}),
    );
    let _ = server.commit(&mut c, tx);
    victims.len()
}
