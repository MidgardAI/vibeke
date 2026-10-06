//! `pane.scroll_requested` (07 §2.6, Batch 2A `pane.scroll`): scrolling is client-side, so the
//! server publishes a scroll request and attached clients move their own view of the pane.
//!
//! The event carries `{offset, total, client}` (offset = rows above the live screen, already
//! clamped by the server). A request naming another client is ignored. For the pane this
//! client has focused, the view moves at once: offset 0 leaves copy mode (back to the live
//! screen), any other offset enters copy mode (or moves the open one) so that the top of the
//! view sits `offset` rows above the live screen, loading history first when needed. A request
//! for a pane that isn't focused here (or while a popup or prompt owns the keyboard) is kept and
//! applied when that pane is next focused in normal mode; a later request for the same pane
//! replaces it, and offset 0 drops it. Nothing is sent to the pane.

use crate::app::{App, Mode};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct State {
    /// (machine, pane) → offset still to apply.
    pub pending: HashMap<(usize, String), u32>,
}

/// A pushed `pane.scroll_requested`.
pub fn on_event(app: &mut App, mi: usize, v: &Value) {
    let Some(pane) = v["subject"]["pane"].as_str() else {
        return;
    };
    if let Some(c) = v["data"]["client"].as_str()
        && !c.is_empty()
        && c != app.client_id
    {
        return;
    }
    let offset = v["data"]["offset"]
        .as_u64()
        .unwrap_or(0)
        .min(u32::MAX as u64) as u32;
    request(app, mi, pane, offset);
}

/// Scroll this client's view of `pane` on machine `mi` to `offset` rows above the live screen.
pub fn request(app: &mut App, mi: usize, pane: &str, offset: u32) {
    let key = (mi, pane.to_string());
    if apply(app, mi, pane, offset) || offset == 0 {
        app.ux.scroll.pending.remove(&key);
    } else {
        app.ux.scroll.pending.insert(key, offset);
    }
    app.dirty = true;
}

/// Apply now when the pane is focused here and the keyboard is free; false = keep it.
fn apply(app: &mut App, mi: usize, pane: &str, offset: u32) -> bool {
    let focused = mi == app.cur && app.focused_pane().as_deref() == Some(pane);
    if !focused {
        return false;
    }
    if matches!(&app.mode, Mode::Copy(cm) if cm.pane == pane) {
        if offset == 0 {
            app.mode = Mode::Normal;
            return true;
        }
        if let Mode::Copy(mut cm) = std::mem::replace(&mut app.mode, Mode::Normal) {
            let out = cm.scroll_to_offset(offset);
            app.copy_outcome(cm, out);
        }
        return true;
    }
    if !matches!(app.mode, Mode::Normal | Mode::Prefix(_)) {
        return false;
    }
    if offset == 0 {
        return true;
    }
    app.enter_copy(None);
    match &mut app.mode {
        Mode::Copy(cm) => {
            cm.want_offset(offset);
            true
        }
        // No screen for the pane yet: try again later.
        _ => false,
    }
}

/// Apply a kept request once its pane is focused in normal mode.
pub fn tick(app: &mut App) {
    if app.ux.scroll.pending.is_empty() {
        return;
    }
    let Some(pane) = app.focused_pane() else {
        return;
    };
    let key = (app.cur, pane.clone());
    if let Some(off) = app.ux.scroll.pending.get(&key).copied()
        && matches!(app.mode, Mode::Normal)
        && apply(app, key.0, &pane, off)
    {
        app.ux.scroll.pending.remove(&key);
    }
    // Forget requests for panes that are gone.
    let machines = &app.machines;
    app.ux.scroll.pending.retain(|(mi, p), _| {
        machines
            .get(*mi)
            .is_some_and(|m| m.model.panes.iter().any(|x| &x.id == p))
    });
}

#[cfg(test)]
#[path = "scroll_req_tests.rs"]
mod tests;
