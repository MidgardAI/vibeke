//! Terminal effects a pane's engine raises that are not pane-local (03 §8): OSC 133 command
//! marks, OSC 9;4 progress, OSC 1337 user vars and OSC 52 clipboard reads.
//!
//! Progress, the last non-zero exit code and user vars live in `SessionModel.pane_live`
//! (in memory only; clients draw them in the sidebar, tab bar and pane frame). Clipboard reads
//! never answer silently: the query goes to one client (the most recently active one showing
//! the pane), which applies `clipboard.osc52_read` (deny by default, ask, allow) on its own
//! machine and answers; only a granted read writes an OSC 52 reply to the pane. Prompt rows
//! and hyperlinks need nothing here: the engine keeps them in its grid (`Row::mark`,
//! `Row::links`).

use crate::{Server, UiEvent};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use vk_proto::model::{ExitMark, PaneLive, Progress, ProgressState};

/// At most this many user vars per pane (further names are dropped).
pub const USER_VARS_MAX: usize = 32;
/// A clipboard query the client has not answered within this long is forgotten.
pub const CLIP_QUERY_TTL: Duration = Duration::from_secs(120);
/// Largest clipboard reply written to a pane.
pub const CLIP_REPLY_MAX: usize = 1 << 20;
/// Open clipboard queries per pane (an app spamming OSC 52 reads gets no more prompts).
const CLIP_QUERIES_PER_PANE: usize = 2;

struct ClipQuery {
    pane: String,
    client: String,
    primary: bool,
    at: Instant,
}

#[derive(Default)]
pub struct State {
    next_req: AtomicU64,
    clip: Mutex<HashMap<u64, ClipQuery>>,
}

/// Mutate (create on demand) a pane's live entry; drops it when it becomes empty. Bumps the
/// model only when something changed.
fn with_live(server: &Server, pane: &str, f: impl FnOnce(&mut PaneLive)) {
    let changed = server.with_core(|c| {
        let list = &mut c.model.pane_live;
        let i = match list.iter().position(|l| l.pane == pane) {
            Some(i) => i,
            None => {
                list.push(PaneLive {
                    pane: pane.to_string(),
                    ..Default::default()
                });
                list.len() - 1
            }
        };
        let before = list[i].clone();
        f(&mut list[i]);
        let changed = list[i] != before;
        if list[i].is_empty() {
            list.remove(i);
        }
        changed
    });
    if changed {
        server.bump_model();
    }
}

/// OSC 9;4: 0 removes, 1 normal (pct), 2 error, 3 indeterminate, 4 paused.
pub fn progress(server: &Server, pane: &str, state: u8, pct: Option<u8>) {
    let st = match state {
        1 => Some(ProgressState::Normal),
        2 => Some(ProgressState::Error),
        3 => Some(ProgressState::Indeterminate),
        4 => Some(ProgressState::Paused),
        _ => None,
    };
    let pct = pct.map(|p| p.min(100));
    with_live(server, pane, |l| {
        l.progress = st.map(|state| Progress {
            state,
            pct: if state == ProgressState::Indeterminate {
                None
            } else {
                pct
            },
        })
    });
}

/// OSC 133 marks: `D;code` with a non-zero code is shown for a few seconds; the next command's
/// output (`C`) clears it.
pub fn mark(server: &Server, pane: &str, kind: char, exit: Option<i32>) {
    match (kind, exit) {
        ('D', Some(code)) if code != 0 => with_live(server, pane, |l| {
            l.last_exit = Some(ExitMark {
                code,
                at_ms: vk_store::now_ms(),
            })
        }),
        ('D', _) | ('C', _) => with_live(server, pane, |l| l.last_exit = None),
        _ => {}
    }
}

/// OSC 1337 SetUserVar (already bounded and sanitized by the engine). An empty value removes
/// the variable.
pub fn user_var(server: &Server, pane: &str, name: String, value: String) {
    with_live(server, pane, |l| {
        match l.user_vars.binary_search_by(|(n, _)| n.as_str().cmp(&name)) {
            Ok(i) if value.is_empty() => {
                l.user_vars.remove(i);
            }
            Ok(i) => l.user_vars[i].1 = value,
            Err(_) if value.is_empty() || l.user_vars.len() >= USER_VARS_MAX => {}
            Err(i) => l.user_vars.insert(i, (name, value)),
        }
    });
}

/// The pane went away: drop its live state and any open clipboard queries.
pub fn forget(server: &Server, pane: &str) {
    with_live(server, pane, |l| *l = PaneLive::default());
    server
        .term_fx
        .clip
        .lock()
        .unwrap()
        .retain(|_, q| q.pane != pane);
}

/// The client that should answer for `pane`: the most recently active one showing it.
fn client_for(server: &Server, pane: &str) -> Option<String> {
    server
        .clients
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, st)| st.kind == "tui" && st.visible.iter().any(|p| p == pane))
        .max_by_key(|(_, st)| st.last_active)
        .map(|(id, _)| id.clone())
}

/// OSC 52 read from a pane. Returns the request id sent to a client, or `None` when no client
/// shows the pane (the read is then denied: nothing is written back).
pub fn clipboard_query(server: &Server, pane: &str, primary: bool) -> Option<u64> {
    let st = &server.term_fx;
    let mut clip = st.clip.lock().unwrap();
    clip.retain(|_, q| q.at.elapsed() < CLIP_QUERY_TTL);
    if clip.values().filter(|q| q.pane == pane).count() >= CLIP_QUERIES_PER_PANE {
        tracing::debug!(pane, "osc52 read: already pending, dropped");
        return None;
    }
    let Some(client) = client_for(server, pane) else {
        tracing::debug!(pane, "osc52 read: no client shows the pane, denied");
        return None;
    };
    let req = st.next_req.fetch_add(1, Ordering::Relaxed) + 1;
    clip.insert(
        req,
        ClipQuery {
            pane: pane.to_string(),
            client: client.clone(),
            primary,
            at: Instant::now(),
        },
    );
    drop(clip);
    let _ = server.ui.send(UiEvent::ClipboardQuery {
        client,
        req,
        pane: pane.to_string(),
        primary,
    });
    Some(req)
}

/// The OSC 52 reply written to the pane for a granted read.
pub fn osc52_reply(data: &[u8], primary: bool) -> Vec<u8> {
    use base64::Engine as _;
    let sel = if primary { 'p' } else { 'c' };
    format!(
        "\x1b]52;{sel};{}\x1b\\",
        base64::engine::general_purpose::STANDARD.encode(data)
    )
    .into_bytes()
}

/// A client's answer to a clipboard query. Only the client the query went to may answer, once,
/// for that pane; `None` (denied) writes nothing. Returns the bytes written to the pane.
pub fn clipboard_reply(
    server: &Server,
    client: &str,
    req: u64,
    pane: &str,
    data: Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let q = {
        let mut clip = server.term_fx.clip.lock().unwrap();
        match clip.get(&req) {
            Some(q) if q.client == client && q.pane == pane && q.at.elapsed() < CLIP_QUERY_TTL => {
                clip.remove(&req)?
            }
            _ => return None,
        }
    };
    crate::audit::clipboard_decision(
        server,
        client,
        pane,
        data.is_some(),
        data.as_ref().map_or(0, Vec::len),
        q.primary,
    );
    let data = data?;
    if data.len() > CLIP_REPLY_MAX {
        return None;
    }
    let bytes = osc52_reply(&data, q.primary);
    let rt = server.pane_rt(pane)?;
    rt.send(crate::pane::PaneCmd::Input {
        id: server.next_internal_input_id(),
        bytes: bytes.clone(),
        ack: None,
    });
    Some(bytes)
}

#[cfg(test)]
#[path = "term_effects_tests.rs"]
mod tests;
