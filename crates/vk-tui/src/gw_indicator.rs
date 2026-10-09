//! The gateway indicator in the status bar: a compact glyph + `gw` (+ device count when
//! online) fed by the server's `GatewayStatus` object — fetched once with `gateway.status`
//! after connecting and kept current by the pushed `gateway.status` event (`crate::push`).
//! Hidden when the gateway is off and not set up for autostart, or when nothing is known.

use crate::app::{App, RpcErr};
use serde_json::Value;

/// Colour role of the indicator; mapped to theme colours by the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Warn,
    Error,
    Neutral,
}

/// The parsed subset of `GatewayStatus` the bar needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayIndicator {
    pub state: String,
    pub autostart: bool,
    pub devices: Option<u32>,
    /// Why the gateway stopped or can't connect (`login_required`: the relay wants a sign-in).
    pub last_error: Option<String>,
}

impl GatewayIndicator {
    /// From a `GatewayStatus` object, or an event envelope carrying it under `data`. `None`
    /// for null or anything without a string `state`.
    pub fn parse(v: &Value) -> Option<Self> {
        let o = if v.get("state").is_some() {
            v
        } else {
            v.get("data").filter(|d| d.get("state").is_some())?
        };
        Some(GatewayIndicator {
            state: o.get("state")?.as_str()?.to_string(),
            autostart: o.get("autostart").and_then(Value::as_bool).unwrap_or(false),
            devices: o.get("devices").and_then(Value::as_u64).map(|n| n as u32),
            last_error: o
                .get("last_error")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        })
    }

    pub fn visible(&self) -> bool {
        self.state != "off" || self.autostart
    }

    pub fn tone(&self) -> Tone {
        match self.state.as_str() {
            "online" | "local_only" => Tone::Ok,
            "starting" | "connecting" | "login_required" => Tone::Warn,
            "offline" | "crashed" => Tone::Error,
            _ => Tone::Neutral,
        }
    }

    /// Bar text; `None` when hidden.
    pub fn render(&self) -> Option<String> {
        if !self.visible() {
            return None;
        }
        Some(match self.state.as_str() {
            "online" => match self.devices {
                Some(n) => format!("● gw {n}"),
                None => "● gw".into(),
            },
            "local_only" => "● gw".into(),
            "starting" | "connecting" => "◌ gw".into(),
            "offline" => "○ gw".into(),
            "crashed" => "✗ gw".into(),
            // Connections → Devices (`prefix+alt+d`) signs in.
            "login_required" => "! gw sign in".into(),
            "external" => "◇ gw".into(),
            _ => "○ gw".into(),
        })
    }
}

/// Machine `mi`'s indicator, when known.
pub fn get(app: &App, mi: usize) -> Option<&GatewayIndicator> {
    app.parity.gateway.get(&mi)
}

/// Replace machine `mi`'s indicator from a status object or event (null clears it).
pub fn set(app: &mut App, mi: usize, v: &Value) {
    match GatewayIndicator::parse(v) {
        Some(g) => {
            app.parity.gateway.insert(mi, g);
        }
        None => {
            app.parity.gateway.remove(&mi);
        }
    }
    app.dirty = true;
}

/// A pushed `gateway.status` event.
pub fn on_event(app: &mut App, mi: usize, v: &Value) {
    set(app, mi, v);
}

/// After connecting: one `gateway.status` fetch (stale data is dropped meanwhile).
pub fn on_connected(app: &mut App, mi: usize) {
    app.parity.gateway.remove(&mi);
    app.command_on(
        mi,
        "gateway.status",
        serde_json::json!({}),
        crate::app::Pending::Parity(crate::parity::Reply::GatewayStatus),
    );
}

/// Reply to `gateway.status`; an old server without the method just shows nothing.
pub fn on_reply(app: &mut App, mi: usize, res: Result<Value, RpcErr>) {
    if let Ok(v) = res {
        set(app, mi, &v);
    }
}

#[cfg(test)]
#[path = "gw_indicator_tests.rs"]
mod tests;
