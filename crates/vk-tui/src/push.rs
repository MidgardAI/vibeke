//! Event push from servers (07 §3): instead of polling `events.read` every second, the TUI
//! subscribes on each render stream (`ClientFrame::Subscribe`) to the events it reacts to and
//! receives them as `ServerFrame::Events`.
//!
//! Only servers whose `render.attach` result lists the `event_push` feature get the frame
//! (an older server would drop the connection on an unknown variant); for those the gateway
//! keeps polling as before. With push, the gateway still makes one `events.read` call after
//! each (re)connect to pick up confirm requests made before the subscription, and again when
//! the server reports that this client lagged.

use crate::app::App;
use serde_json::{Value, json};
use std::time::Instant;
use vk_proto::render::{ClientFrame, PushedEvent};

pub const FEATURE: &str = "event_push";

/// What the TUI subscribes to.
pub const TYPES: &[&str] = &[
    "client.confirm_*",
    "client.attached",
    "client.detached",
    "client.devices_changed",
    "interaction.*",
    "browser.session_*",
    "browser.taken_over",
    "browser.released",
    "screenshot.*",
    "preview.console_error",
    "draft.*",
    "notes.updated",
    "assistant.*",
    "client.window_title_changed",
    "plugin.registry_changed",
    "plugin.agent_view_changed",
    "ui.contributions_changed",
    // v1 TUI: elevation requests (09 §3.2) and scroll requests (07 §2.6).
    "auth.elevate_*",
    "pane.scroll_requested",
];

#[derive(Debug, Default, Clone)]
pub struct Per {
    pub active: bool,
    pub cursor: i64,
    pub received: u64,
}

#[derive(Debug, Default)]
pub struct State {
    pub per: Vec<Per>,
}

impl State {
    pub fn per(&mut self, i: usize) -> &mut Per {
        if self.per.len() <= i {
            self.per.resize_with(i + 1, Per::default);
        }
        &mut self.per[i]
    }
    pub fn active(&self, i: usize) -> bool {
        self.per.get(i).is_some_and(|p| p.active)
    }
}

pub fn supported(app: &App, i: usize) -> bool {
    app.machines[i].features.iter().any(|f| f == FEATURE)
}

/// After (re)connecting machine `i`: subscribe when the server supports it, else poll.
pub fn on_connected(app: &mut App, i: usize) {
    let on = supported(app, i);
    let cursor = {
        let p = app.push.per(i);
        p.active = on;
        p.cursor
    };
    crate::gateway::set_push(app, i, on);
    if on {
        app.machines[i].send(ClientFrame::Subscribe {
            types: TYPES.iter().map(|s| s.to_string()).collect(),
            after: (cursor > 0).then_some(cursor),
        });
    }
}

/// Route pushed events.
pub fn on_events(app: &mut App, i: usize, events: Vec<PushedEvent>, lagged: bool) {
    let mut confirms = Vec::new();
    let mut refresh_clients = false;
    let mut refresh_sessions = false;
    let mut inbox = false;
    for e in events {
        {
            let p = app.push.per(i);
            p.cursor = p.cursor.max(e.seq);
            p.received += 1;
        }
        let v: Value = serde_json::from_str(&e.json).unwrap_or(Value::Null);
        let k = e.kind.as_str();
        if k.starts_with("client.confirm_") {
            confirms.push(v);
        } else if matches!(
            k,
            "client.attached" | "client.detached" | "client.devices_changed"
        ) {
            refresh_clients = true;
        } else if k.starts_with("interaction.") {
            inbox = true;
        } else if k.starts_with("browser.") {
            refresh_sessions = true;
        } else if k == "screenshot.captured" {
            let what = v["data"]["url"]
                .as_str()
                .or_else(|| v["subject"]["browser_session"].as_str())
                .unwrap_or("")
                .to_string();
            app.toast(
                format!("📷 screenshot captured {what}")
                    .trim_end()
                    .to_string(),
            );
            crate::gallery::on_captured(app, i, &v);
            crate::preview_ui::on_captured(app, i, &v);
        } else if k == "preview.console_error" {
            crate::preview_ui::on_console_error(app, i, &v);
        } else if k == "screenshot.deleted" {
            crate::gallery::on_deleted(app, i);
        } else if k.starts_with("draft.") || k == "notes.updated" {
            crate::drafts::on_event(app, i, k, &v);
        } else if k.starts_with("assistant.") {
            crate::assist::on_event(app, i, k);
        } else if k == "client.window_title_changed" {
            crate::plugins::on_title_event(app, i, &v);
        } else if k == "plugin.registry_changed"
            || k == "plugin.agent_view_changed"
            || k == "ui.contributions_changed"
        {
            crate::plugins::on_registry_event(app, i);
        } else if k.starts_with("auth.elevate_") {
            crate::elevate::on_event(app, i, k, &v);
        } else if k == "pane.scroll_requested" {
            crate::scroll_req::on_event(app, i, &v);
        } else if k.starts_with("task.collision_") {
            crate::collision::on_event(app, i, k);
        }
    }
    if !confirms.is_empty() {
        crate::gateway::on_events(
            app,
            i,
            &json!({"events": confirms}),
            now_ms(),
            Instant::now(),
        );
    }
    if refresh_clients {
        crate::gateway::refresh_list(app, i);
    }
    if refresh_sessions {
        crate::nav::refresh_sessions(app, i);
    }
    if inbox {
        crate::inbox::invalidate(app);
    }
    if lagged {
        crate::gateway::catch_up(app, i);
    }
    app.dirty = true;
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "push_tests.rs"]
mod tests;
