//! Jujutsu display state per pane (08 §2.1, M4): `Pane.jj` holds the working-copy change's
//! bookmarks (or short change id) for panes whose cwd is inside a jj repo.
//!
//! Event-driven like the rest of the idle budget (spec 10 §1.3): a refresh runs on
//! `pane.created`, `pane.cwd_changed` and `pane.process_changed` (a `jj` command finishing in a
//! shell changes the foreground process), debounced; nothing polls. The query itself is
//! `vk_tasks::jj_display_label` (read-only, hardened, short timeout) on a blocking thread. A
//! missing `jj` binary yields `None` everywhere.

use crate::Server;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const DEBOUNCE: Duration = Duration::from_millis(250);

pub fn start(server: &Arc<Server>) {
    tokio::spawn(run(server.clone()));
}

async fn run(server: Arc<Server>) {
    let mut events = server.events.subscribe();
    // Panes restored at startup get their label once.
    let mut pending: HashSet<String> = server.with_core(|c| {
        c.model
            .panes
            .iter()
            .filter(|p| !p.exited && !p.is_browser())
            .map(|p| p.id.clone())
            .collect()
    });
    loop {
        let wait = if pending.is_empty() {
            Duration::from_secs(3600)
        } else {
            DEBOUNCE
        };
        tokio::select! {
            ev = events.recv() => match ev {
                Ok(e) if matches!(e.kind.as_str(), "pane.created" | "pane.cwd_changed" | "pane.process_changed") => {
                    if let Some(p) = e.subject.get("pane").and_then(|v| v.as_str()) {
                        pending.insert(p.to_string());
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            },
            _ = tokio::time::sleep(wait), if !pending.is_empty() => {
                let batch: Vec<String> = pending.drain().collect();
                refresh(&server, batch).await;
            }
        }
    }
}

/// Recompute `Pane.jj` for `panes` and commit the ones that changed.
pub async fn refresh(server: &Arc<Server>, panes: Vec<String>) {
    refresh_with(server, panes, vk_tasks::jj_display_bin()).await
}

/// [`refresh`] with an explicit `jj` binary (tests).
pub async fn refresh_with(server: &Arc<Server>, panes: Vec<String>, bin: PathBuf) {
    let mut cwds: Vec<(String, PathBuf)> = Vec::new();
    for id in panes {
        let live = server.with_core(|c| c.pane(&id).is_some_and(|p| !p.exited && !p.is_browser()));
        if !live {
            continue;
        }
        let stored = server.with_core(|c| c.pane(&id).and_then(|p| p.cwd.clone()));
        if let Some(cwd) = server.pane_cwd(&id).or(stored) {
            cwds.push((id, PathBuf::from(cwd)));
        }
    }
    if cwds.is_empty() {
        return;
    }
    let labels: Vec<(String, Option<String>)> = tokio::task::spawn_blocking(move || {
        // Panes in the same repo share one query.
        let mut by_root: HashMap<PathBuf, Option<String>> = HashMap::new();
        cwds.into_iter()
            .map(|(id, cwd)| {
                let label = match vk_tasks::jj_root(&cwd) {
                    None => None,
                    Some(root) => by_root
                        .entry(root)
                        .or_insert_with_key(|r| vk_tasks::jj_display_label(&bin, r))
                        .clone(),
                };
                (id, label)
            })
            .collect()
    })
    .await
    .unwrap_or_default();
    for (id, label) in labels {
        let mut c = server.core.lock().unwrap();
        if let Some(mut p) = c.pane(&id).cloned()
            && p.jj != label
        {
            p.jj = label;
            let mut tx = crate::core::Tx::new();
            tx.pane(p);
            let _ = server.commit(&mut c, tx);
        }
    }
}

#[cfg(test)]
#[path = "vcs_tests.rs"]
mod tests;
