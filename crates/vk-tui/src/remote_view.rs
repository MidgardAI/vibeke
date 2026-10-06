//! Remote machines in the client (06 A5/A7): link state in the sidebar (`● devbox 23ms`,
//! `◐ devbox degraded 512ms`, `○ devbox offline · last seen 4m ago`), adaptive frame pacing,
//! and the "while you were away" replay after a reconnect.
//!
//! **Pacing.** The render stream already holds at most two unacked frames per pane, so the
//! client paces a pane by when it acks: frames of an *unfocused* pane on a remote machine are
//! acked at most every 250 ms, which caps that pane at 4 Hz in steady state without any
//! server change and without dropping state (frames are state-sync; the server coalesces).
//! The focused pane is acked at once and attaches with
//! `max_fps = min(client_hz, 1000 / (RTT/2 + 8 ms))` ([`target_hz`]), re-evaluated on every
//! reconnect.
//!
//! **Replay.** While connected the client remembers the remote's event head (`server.status`
//! at connect, then every pushed event). After a reconnect it reads the agent events it
//! missed and shows one notification per machine, collapsed to the latest state per pane:
//! "[devbox] While you were away: 2 agents finished, 1 needs approval".

use crate::app::{App, Pending, RpcErr};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vk_proto::render::ClientFrame;

/// Steady-state cap for unfocused remote panes (06 A7).
pub const UNFOCUSED_HZ: u32 = 4;
/// The client's own refresh cap for remote machines.
pub const CLIENT_HZ: u32 = 60;

/// A link snapshot for the sidebar and pacing (from `vk_remote::Link::status`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkInfo {
    /// connected | degraded | reconnecting | offline
    pub state: String,
    pub rtt_ms: Option<u64>,
    /// Wall clock (unix ms) of the last frame from the machine.
    pub last_seen_ms: Option<u64>,
}

/// Reads the current [`LinkInfo`] of a machine's link.
pub type LinkProbe = Arc<dyn Fn() -> LinkInfo + Send + Sync>;

/// `min(client_hz, 1000 / (RTT/2 + 8 ms))` (06 A7); unknown RTT keeps `client_hz`.
pub fn target_hz(client_hz: u32, rtt_ms: Option<u64>) -> u32 {
    let Some(rtt) = rtt_ms else {
        return client_hz.max(1);
    };
    let per_frame = rtt / 2 + 8;
    let hz = (1000 / per_frame.max(1)) as u32;
    client_hz.min(hz).max(1)
}

/// A reconnect backoff factor in [0.8, 1.2) (06 A7 "with jitter"), so clients of a machine
/// that went away do not all come back in lockstep.
pub fn jitter() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let r = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    0.8 + 0.4 * ((r % 10_000) as f64 / 10_000.0)
}

/// Minimum spacing of acks for an unfocused remote pane: one ack lets the server send one
/// more frame, so this spacing is the steady-state frame interval.
pub fn unfocused_ack_interval() -> Duration {
    Duration::from_millis(1000 / UNFOCUSED_HZ as u64)
}

#[derive(Debug, Default)]
struct PaneAcks {
    last: Option<Instant>,
    /// Acks not sent yet, oldest first (each one releases one frame on the server).
    owed: Vec<(u32, u64)>,
}

#[derive(Default)]
pub struct State {
    acks: HashMap<(usize, String), PaneAcks>,
    /// Remote event head per machine while connected (seq), and the cursor saved at
    /// disconnect for the replay.
    head: HashMap<usize, i64>,
    away_from: HashMap<usize, i64>,
    /// Wall clock of the last disconnect per machine (for "last seen" without a probe).
    lost_at_ms: HashMap<usize, u64>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn is_focused(app: &App, mi: usize, pane: &str) -> bool {
    app.cur == mi && app.m().focus.pane.as_deref() == Some(pane)
}

/// Ack a frame of `pane` on machine `mi`, now or later (pacing, see module docs).
pub fn ack(app: &mut App, mi: usize, pane: String, epoch: u32, rev: u64) {
    let local = app.machines.get(mi).is_none_or(|m| m.local);
    if local || is_focused(app, mi, &pane) {
        app.remote.acks.remove(&(mi, pane.clone()));
        app.machines[mi].send(ClientFrame::Ack { pane, epoch, rev });
        return;
    }
    let now = Instant::now();
    let e = app.remote.acks.entry((mi, pane.clone())).or_default();
    let due = e.last.is_none_or(|t| now >= t + unfocused_ack_interval());
    if due && e.owed.is_empty() {
        e.last = Some(now);
        app.machines[mi].send(ClientFrame::Ack { pane, epoch, rev });
    } else {
        e.owed.push((epoch, rev));
    }
}

/// Send acks that became due, and every owed ack of a pane that is focused now.
pub fn release_due(app: &mut App, now: Instant) {
    if app.remote.acks.is_empty() {
        return;
    }
    let keys: Vec<(usize, String)> = app.remote.acks.keys().cloned().collect();
    for (mi, pane) in keys {
        let focused = is_focused(app, mi, &pane);
        let connected = app.machines.get(mi).is_some_and(|m| m.connected());
        let Some(e) = app.remote.acks.get_mut(&(mi, pane.clone())) else {
            continue;
        };
        if !connected {
            app.remote.acks.remove(&(mi, pane));
            continue;
        }
        let mut send = Vec::new();
        if focused {
            send = std::mem::take(&mut e.owed);
        } else if !e.owed.is_empty() && e.last.is_none_or(|t| now >= t + unfocused_ack_interval()) {
            send.push(e.owed.remove(0));
            e.last = Some(now);
        }
        let empty = e.owed.is_empty();
        for (epoch, rev) in send {
            app.machines[mi].send(ClientFrame::Ack {
                pane: pane.clone(),
                epoch,
                rev,
            });
        }
        if empty && focused {
            app.remote.acks.remove(&(mi, pane));
        }
    }
}

/// When owed acks fall due.
pub fn deadlines(app: &App, d: &mut crate::deadline::Deadlines) {
    let next = app
        .remote
        .acks
        .values()
        .filter(|e| !e.owed.is_empty())
        .filter_map(|e| e.last.map(|t| t + unfocused_ack_interval()))
        .min();
    if let Some(t) = next {
        d.at("remote_pacing", t);
    }
}

/// Acks owed for a pane (tests, `debug`).
pub fn owed(app: &App, mi: usize, pane: &str) -> usize {
    app.remote
        .acks
        .get(&(mi, pane.to_string()))
        .map_or(0, |e| e.owed.len())
}

// ---- sidebar ------------------------------------------------------------------------------

/// "just now", "42s ago", "4m ago", "3h ago", "2d ago".
pub fn ago(now_ms: u64, then_ms: u64) -> String {
    let s = now_ms.saturating_sub(then_ms) / 1000;
    match s {
        0..=4 => "just now".into(),
        5..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// What follows the machine label in the sidebar, and whether the link is degraded.
pub fn status_suffix(app: &App, mi: usize) -> (String, bool) {
    let m = &app.machines[mi];
    let info = m.link.as_ref().map(|p| p());
    if m.connected() {
        return match info {
            Some(i) if i.state == "degraded" => (
                format!(
                    " degraded{}",
                    i.rtt_ms.map(|r| format!(" {r}ms")).unwrap_or_default()
                ),
                true,
            ),
            Some(i) => (
                i.rtt_ms.map(|r| format!(" {r}ms")).unwrap_or_default(),
                false,
            ),
            None => (String::new(), false),
        };
    }
    let seen = info
        .and_then(|i| i.last_seen_ms)
        .or_else(|| app.remote.lost_at_ms.get(&mi).copied());
    let tail = match seen {
        Some(t) if !m.local => format!(" · last seen {}", ago(now_ms(), t)),
        _ => String::new(),
    };
    (format!(" {}{tail}", m.status), false)
}

// ---- reconnect replay ---------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Reply {
    /// `server.status`: the event head at connect.
    Head,
    /// `events.read` after the saved cursor.
    Away,
}

/// Agent events the replay reads.
pub const AWAY_TYPES: &[&str] = &["agent.state_changed", "interaction.opened"];

pub fn on_connected(app: &mut App, i: usize) {
    if app.machines[i].local {
        return;
    }
    if let Some(after) = app.remote.away_from.remove(&i) {
        app.command_on(
            i,
            "events.read",
            json!({"after": after, "types": AWAY_TYPES, "limit": 500}),
            Pending::Remote(Reply::Away),
        );
    }
    app.command_on(i, "server.status", json!({}), Pending::Remote(Reply::Head));
}

pub fn on_disconnected(app: &mut App, i: usize) {
    app.remote.acks.retain(|(mi, _), _| *mi != i);
    if app.machines[i].local {
        return;
    }
    app.remote.lost_at_ms.insert(i, now_ms());
    let pushed = if crate::push::supported(app, i) {
        app.push.per.get(i).map_or(0, |p| p.cursor)
    } else {
        0
    };
    if let Some(h) = app.remote.head.remove(&i) {
        let from = h.max(pushed);
        // Keep the oldest cursor across repeated drops without a successful replay.
        let e = app.remote.away_from.entry(i).or_insert(from);
        *e = (*e).min(from);
    }
}

pub fn on_reply(app: &mut App, i: usize, r: Reply, res: Result<Value, RpcErr>) {
    let Ok(v) = res else {
        return; // an older server or a dropped link: no replay, no noise
    };
    match r {
        Reply::Head => {
            if let Some(seq) = v.get("event_seq").and_then(Value::as_i64) {
                app.remote.head.insert(i, seq);
            }
        }
        Reply::Away => {
            if let Some(text) = summarize(v.get("events").unwrap_or(&Value::Null)) {
                let label = app.machines[i].label.clone();
                crate::notifications::on_notify(
                    app,
                    i,
                    "While you were away".into(),
                    text.clone(),
                    None,
                    Vec::new(),
                );
                // Keep it even when there is a single machine (on_notify omits the badge then).
                if app.machines.len() == 1 {
                    app.toast(format!("[{label}] While you were away: {text}"));
                }
            }
        }
    }
}

/// Collapse missed agent events to the latest state per pane and count them:
/// `2 agents finished, 1 needs approval`. `None` when nothing noteworthy happened.
pub fn summarize(events: &Value) -> Option<String> {
    let mut last: HashMap<String, &'static str> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for e in events.as_array().into_iter().flatten() {
        let kind = e["type"].as_str().unwrap_or("");
        let pane = e["subject"]["pane"]
            .as_str()
            .or_else(|| e["subject"]["run"].as_str())
            .unwrap_or("")
            .to_string();
        let what = match kind {
            "interaction.opened" => match e["data"]["kind"].as_str() {
                Some("question") => Some("needs an answer"),
                _ => Some("needs approval"),
            },
            "agent.state_changed" => {
                let to = e["data"]["to"].as_str().unwrap_or("");
                let from = e["data"]["from"].as_str().unwrap_or("");
                match to {
                    "idle" if from == "working" => Some("finished"),
                    "error" | "rate_limited" => Some("failed"),
                    "exited" => Some("exited"),
                    "working" => Some("working"),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(w) = what {
            if !last.contains_key(&pane) {
                order.push(pane.clone());
            }
            last.insert(pane, w);
        }
    }
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for p in &order {
        let w = last[p];
        if w == "working" {
            continue; // still running: not news
        }
        match counts.iter_mut().find(|(k, _)| *k == w) {
            Some((_, n)) => *n += 1,
            None => counts.push((w, 1)),
        }
    }
    if counts.is_empty() {
        return None;
    }
    // Attention first.
    let rank = |w: &str| match w {
        "needs approval" => 0,
        "needs an answer" => 1,
        "failed" => 2,
        "finished" => 3,
        _ => 4,
    };
    counts.sort_by_key(|(w, _)| rank(w));
    Some(
        counts
            .iter()
            .map(|(w, n)| {
                let noun = if *n == 1 { "agent" } else { "agents" };
                match *w {
                    "needs approval" | "needs an answer" if *n == 1 => format!("1 {w}"),
                    "needs approval" => format!("{n} need approval"),
                    "needs an answer" => format!("{n} need an answer"),
                    w => format!("{n} {noun} {w}"),
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

#[cfg(test)]
#[path = "remote_view_tests.rs"]
mod tests;
