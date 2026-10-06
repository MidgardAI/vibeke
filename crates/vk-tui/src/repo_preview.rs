//! A trusted repo's `[preview]` settings in the TUI (08 §11.1, 06 Part C).
//!
//! When a workspace's repo-local `.vibeke/config.toml` is trusted (`crate::trust`), its
//! `[preview]` keys are layered over this client's own `[preview]` (repo keys win, key by key;
//! an invalid repo value falls back to the user's for that key). The result applies to previews
//! of that workspace (the preview's pane, else its task's workspace, else the focused one):
//!
//! - `mode`: `pane` opens a browser pane, `window` the profile browser window, `proxy` the
//!   authenticated proxy URL in the normal browser — for `enter` on a preview chip, row or goto
//!   entry, and `open_preview`.
//! - `pane_split`: where the browser pane goes (`right`, `down`, `tab`, `float`).
//! - `inline_thumbnails = false` turns the sidebar thumbnail off for that repo's previews.
//!
//! Untrusted or changed files never apply (the trust check is the server's digest of the whole
//! `.vibeke/` tree). Server-side preview keys (discovery, browser binaries, egress) are read by
//! the server from its own config.

use crate::app::App;
use vk_proto::model::Preview;

/// The layered `[preview]` for a trusted repo `info` (None when nothing applies).
pub fn layered(app: &App, info: &crate::trust::Info) -> Option<vk_config::Preview> {
    if !info.trusted {
        return None;
    }
    let text = info.text.as_deref()?;
    let rc = vk_config::repo::parse(text, std::path::Path::new(&info.repo)).ok()?;
    rc.preview.as_ref()?;
    app.config.with_repo(&rc).ok().map(|c| c.preview())
}

/// The workspace a preview belongs to: its pane's, else its task's, else the focused one when
/// it is on the current machine.
pub fn ws_of_preview(app: &App, mi: usize, p: &Preview) -> Option<String> {
    let m = app.machines.get(mi)?;
    if let Some(ws) = p
        .pane
        .as_ref()
        .and_then(|id| m.model.panes.iter().find(|x| &x.id == id))
        .map(|x| x.workspace.clone())
    {
        return Some(ws);
    }
    if let Some(ws) = p
        .task
        .as_ref()
        .and_then(|id| m.model.tasks.iter().find(|t| &t.id == id))
        .and_then(|t| t.workspace.clone())
    {
        return Some(ws);
    }
    (mi == app.cur).then(|| m.focus.workspace.clone()).flatten()
}

/// The effective `[preview]` for workspace `ws` of machine `mi`.
pub fn effective(app: &App, mi: usize, ws: Option<&str>) -> vk_config::Preview {
    ws.and_then(|w| app.ux.trust.info.get(&(mi, w.to_string())))
        .filter(|i| i.trusted)
        .and_then(|i| i.preview.clone())
        .unwrap_or_else(|| app.config.preview())
}

/// The effective `[preview]` for preview `p` of machine `mi`.
pub fn for_preview(app: &App, mi: usize, p: &Preview) -> vk_config::Preview {
    effective(app, mi, ws_of_preview(app, mi, p).as_deref())
}

#[cfg(test)]
#[path = "repo_preview_tests.rs"]
mod tests;
