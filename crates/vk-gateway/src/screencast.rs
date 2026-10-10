//! Live browser previews for apps (spec 16 §7.4): the server holds a screencast subscription per
//! client connection and session, and this gateway is one client for every device. So the gateway
//! counts the devices watching each session itself: it attaches once, and detaches when the last
//! device detaches, stops polling for frames (`LEASE`) or disconnects. A take-over made from a
//! device ends with that device's lease too, so a phone that went away never leaves the agent
//! blocked on `human_control`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Gateway;
use crate::api::{ApiError, ApiResult};

/// A device that has not polled a frame for this long stops watching.
pub const LEASE: Duration = Duration::from_secs(30);
/// How often live sessions are attached again: the server drops the gateway's subscriptions when
/// its connection to the server ends, and a fresh attach restores them.
const REATTACH: Duration = Duration::from_secs(15);

#[derive(Default)]
pub struct Screencasts {
    map: Mutex<HashMap<String, Watch>>,
    /// Serializes attach, detach and sweep sequences with the server.
    seq: tokio::sync::Mutex<()>,
    reattached: Mutex<Option<Instant>>,
}

#[derive(Default)]
struct Watch {
    /// Device id → last attach or frame poll.
    viewers: HashMap<String, Instant>,
    /// The device whose take-over is in force.
    taken_by: Option<String>,
}

/// What to undo on the server after devices stopped watching a session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Release {
    /// The last viewer left: detach the gateway's screencast subscription.
    pub detach: bool,
    /// The device that took the session over left: hand control back to the agent.
    pub release: bool,
}

impl Screencasts {
    pub fn add_viewer(&self, session: &str, device: &str, now: Instant) {
        self.map
            .lock()
            .unwrap()
            .entry(session.to_string())
            .or_default()
            .viewers
            .insert(device.to_string(), now);
    }

    /// Renew a device's lease on a frame poll. False when the device is not watching.
    pub fn touch(&self, session: &str, device: &str, now: Instant) -> bool {
        match self
            .map
            .lock()
            .unwrap()
            .get_mut(session)
            .and_then(|w| w.viewers.get_mut(device))
        {
            Some(t) => {
                *t = now;
                true
            }
            None => false,
        }
    }

    pub fn set_taken(&self, session: &str, device: Option<&str>) {
        let mut m = self.map.lock().unwrap();
        match device {
            Some(d) => m.entry(session.to_string()).or_default().taken_by = Some(d.to_string()),
            None => {
                if let Some(w) = m.get_mut(session) {
                    w.taken_by = None;
                    if w.viewers.is_empty() {
                        m.remove(session);
                    }
                }
            }
        }
    }

    pub fn taken_by(&self, session: &str) -> Option<String> {
        self.map
            .lock()
            .unwrap()
            .get(session)
            .and_then(|w| w.taken_by.clone())
    }

    /// Remove the viewers `gone` selects (`Some(last poll)`) from every session; returns what to
    /// undo per session. A take-over ends with its device: when that device's lease is removed,
    /// or, for a device that took over without watching, when `gone` selects it with `None`.
    fn remove_where(
        &self,
        gone: impl Fn(&str, &str, Option<Instant>) -> bool,
    ) -> Vec<(String, Release)> {
        let mut m = self.map.lock().unwrap();
        let mut out = vec![];
        for (session, w) in m.iter_mut() {
            let removed: Vec<String> = w
                .viewers
                .iter()
                .filter(|(d, t)| gone(session, d, Some(**t)))
                .map(|(d, _)| d.clone())
                .collect();
            let before = w.viewers.len();
            let release = w.taken_by.as_deref().is_some_and(|d| {
                removed.iter().any(|r| r == d)
                    || (!w.viewers.contains_key(d) && gone(session, d, None))
            });
            w.viewers.retain(|d, _| !removed.contains(d));
            let detach = before > 0 && w.viewers.is_empty();
            if release {
                w.taken_by = None;
            }
            if detach || release {
                out.push((session.clone(), Release { detach, release }));
            }
        }
        m.retain(|_, w| !w.viewers.is_empty() || w.taken_by.is_some());
        out
    }

    pub fn remove_viewer(&self, session: &str, device: &str) -> Release {
        self.remove_where(|s, d, _| s == session && d == device)
            .into_iter()
            .find(|(s, _)| s == session)
            .map(|(_, r)| r)
            .unwrap_or_default()
    }

    pub fn expire(&self, now: Instant) -> Vec<(String, Release)> {
        self.remove_where(|_, _, t| t.is_some_and(|t| now.duration_since(t) >= LEASE))
    }

    pub fn device_gone(&self, device: &str) -> Vec<(String, Release)> {
        self.remove_where(|_, d, _| d == device)
    }

    /// Sessions someone is watching.
    pub fn live(&self) -> Vec<String> {
        self.map
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, w)| !w.viewers.is_empty())
            .map(|(s, _)| s.clone())
            .collect()
    }
}

const ACTOR: &str = "gateway:preview-lease";

async fn undo(gw: &Gateway, session: &str, r: Release) {
    if r.release {
        let _ = gw
            .server
            .call_as(ACTOR, "browser.release", json!({"session": session}))
            .await;
    }
    if r.detach {
        let _ = gw
            .server
            .call_as(
                ACTOR,
                "browser.detach_screencast",
                json!({"session": session}),
            )
            .await;
    }
}

/// `browser.attach_screencast` for one device: attach on the server, then count the device.
pub async fn attach(gw: &Gateway, actor: &str, device: &str, session: &str) -> ApiResult {
    let _seq = gw.screencasts.seq.lock().await;
    let r = gw
        .server
        .call_as(
            actor,
            "browser.attach_screencast",
            json!({"session": session}),
        )
        .await?;
    // Count the device under the handle the server answers with; frame polls and detach use it.
    let handle = r
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or(session)
        .to_string();
    gw.screencasts.add_viewer(&handle, device, Instant::now());
    Ok(r)
}

/// `browser.detach_screencast` for one device: the server subscription ends with the last viewer.
pub async fn detach(gw: &Gateway, device: &str, session: &str) -> ApiResult {
    let _seq = gw.screencasts.seq.lock().await;
    let r = gw.screencasts.remove_viewer(session, device);
    undo(gw, session, r).await;
    Ok(json!({"session": session, "detached": true}))
}

/// `browser.screencast_frame`: renews the device's lease, then reads the latest frame.
pub async fn frame(gw: &Gateway, device: &str, params: Value) -> ApiResult {
    let session = params.get("session").and_then(|v| v.as_str()).unwrap_or("");
    if !gw.screencasts.touch(session, device, Instant::now()) {
        return Err(ApiError::new(
            "conflict",
            "attach to this session's screencast first (browser.attach_screencast)",
        ));
    }
    gw.server.call("browser.screencast_frame", params).await
}

/// A device's last connection closed: it stops watching everything.
pub async fn device_gone(gw: &Arc<Gateway>, device: &str) {
    let _seq = gw.screencasts.seq.lock().await;
    for (session, r) in gw.screencasts.device_gone(device) {
        undo(gw, &session, r).await;
    }
}

/// Periodic upkeep: expire leases of devices that stopped polling, and attach live sessions
/// again so a reconnected server connection keeps its screencasts.
pub async fn sweep(gw: &Arc<Gateway>) {
    let _seq = gw.screencasts.seq.lock().await;
    let now = Instant::now();
    for (session, r) in gw.screencasts.expire(now) {
        undo(gw, &session, r).await;
    }
    let due = {
        let mut last = gw.screencasts.reattached.lock().unwrap();
        let due = last.is_none_or(|t| now.duration_since(t) >= REATTACH);
        if due {
            *last = Some(now);
        }
        due
    };
    if due {
        for session in gw.screencasts.live() {
            let _ = gw
                .server
                .call_as(
                    ACTOR,
                    "browser.attach_screencast",
                    json!({"session": session}),
                )
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_viewer_detaches_and_a_takeover_ends_with_its_device() {
        let s = Screencasts::default();
        let t0 = Instant::now();
        s.add_viewer("b1", "phone", t0);
        s.add_viewer("b1", "tablet", t0);
        s.set_taken("b1", Some("phone"));
        assert_eq!(s.taken_by("b1").as_deref(), Some("phone"));
        // The phone leaves: control goes back, but the tablet still watches.
        assert_eq!(
            s.remove_viewer("b1", "phone"),
            Release {
                detach: false,
                release: true
            }
        );
        assert_eq!(s.taken_by("b1"), None);
        // The last viewer leaves: detach.
        assert_eq!(
            s.remove_viewer("b1", "tablet"),
            Release {
                detach: true,
                release: false
            }
        );
        assert!(s.live().is_empty());
        // Detaching a session nobody watches changes nothing.
        assert_eq!(s.remove_viewer("b1", "tablet"), Release::default());
    }

    #[test]
    fn leases_expire_without_frame_polls() {
        let s = Screencasts::default();
        let t0 = Instant::now();
        s.add_viewer("b1", "phone", t0);
        s.add_viewer("b2", "phone", t0);
        s.set_taken("b2", Some("phone"));
        assert!(s.touch("b1", "phone", t0 + LEASE / 2));
        assert!(!s.touch("b1", "laptop", t0), "only a viewer renews");
        let mut gone = s.expire(t0 + LEASE + Duration::from_secs(1));
        gone.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            gone,
            vec![(
                "b2".to_string(),
                Release {
                    detach: true,
                    release: true
                }
            )]
        );
        assert_eq!(s.live(), vec!["b1".to_string()]);
        let gone = s.expire(t0 + LEASE * 2);
        assert_eq!(gone.len(), 1);
        assert!(s.live().is_empty());
    }

    #[test]
    fn a_disconnected_device_leaves_every_session() {
        let s = Screencasts::default();
        let t0 = Instant::now();
        s.add_viewer("b1", "phone", t0);
        s.add_viewer("b2", "phone", t0);
        s.add_viewer("b2", "tablet", t0);
        let mut gone = s.device_gone("phone");
        gone.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].0, "b1");
        assert!(gone[0].1.detach);
        assert_eq!(s.live(), vec!["b2".to_string()]);
        // A take-over without watching ends when that device goes away.
        s.set_taken("b3", Some("phone"));
        let gone = s.device_gone("phone");
        assert_eq!(
            gone,
            vec![(
                "b3".to_string(),
                Release {
                    detach: false,
                    release: true
                }
            )]
        );
        assert_eq!(s.taken_by("b3"), None);
    }
}
