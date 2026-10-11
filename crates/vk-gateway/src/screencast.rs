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
/// Phone previews (`browser.preview_open`): largest viewport side in CSS px, largest DPR, and
/// most open per device.
pub const MAX_SIDE: u64 = 2048;
pub const MAX_DPR: f64 = 4.0;
pub const MAX_PER_DEVICE: usize = 3;
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
    /// The device whose take-over is in force or in flight: its lease end releases the session.
    taken_by: Option<String>,
    /// The host confirmed `taken_by`'s take-over: only then does its input reach the page.
    confirmed: bool,
    /// The device that opened this session with `browser.preview_open` (06 B3.6): the session
    /// closes on the host when that device's lease ends.
    opened_by: Option<String>,
}

/// What to undo on the server after devices stopped watching a session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Release {
    /// The last viewer left: detach the gateway's screencast subscription.
    pub detach: bool,
    /// The device that took the session over left: hand control back to the agent.
    pub release: bool,
    /// The device that opened the session left: close it on the host.
    pub close: bool,
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

    /// Record that `device` opened `session` (a phone preview); its lease end closes it.
    pub fn set_opened(&self, session: &str, device: &str) {
        self.map
            .lock()
            .unwrap()
            .entry(session.to_string())
            .or_default()
            .opened_by = Some(device.to_string());
    }

    /// How many sessions `device` opened that are still open here.
    pub fn opened_count(&self, device: &str) -> usize {
        self.map
            .lock()
            .unwrap()
            .values()
            .filter(|w| w.opened_by.as_deref() == Some(device))
            .count()
    }

    pub fn opened_by(&self, session: &str) -> Option<String> {
        self.map
            .lock()
            .unwrap()
            .get(session)
            .and_then(|w| w.opened_by.clone())
    }

    pub fn set_taken(&self, session: &str, device: Option<&str>) {
        let mut m = self.map.lock().unwrap();
        match device {
            Some(d) => {
                let w = m.entry(session.to_string()).or_default();
                if w.taken_by.as_deref() != Some(d) {
                    w.taken_by = Some(d.to_string());
                    w.confirmed = false;
                }
            }
            None => {
                if let Some(w) = m.get_mut(session) {
                    w.taken_by = None;
                    w.confirmed = false;
                    if w.viewers.is_empty() {
                        m.remove(session);
                    }
                }
            }
        }
    }

    pub fn is_viewer(&self, session: &str, device: &str) -> bool {
        self.map
            .lock()
            .unwrap()
            .get(session)
            .is_some_and(|w| w.viewers.contains_key(device))
    }

    /// The host answered the take-over: `device` now controls the page.
    pub fn confirm_taken(&self, session: &str, device: &str) {
        let mut m = self.map.lock().unwrap();
        let w = m.entry(session.to_string()).or_default();
        w.taken_by = Some(device.to_string());
        w.confirmed = true;
    }

    /// The device whose confirmed take-over lets its input reach the page.
    pub fn controller(&self, session: &str) -> Option<String> {
        self.map
            .lock()
            .unwrap()
            .get(session)
            .filter(|w| w.confirmed)
            .and_then(|w| w.taken_by.clone())
    }

    fn take_state(&self, session: &str) -> Option<(String, bool)> {
        let m = self.map.lock().unwrap();
        let w = m.get(session)?;
        Some((w.taken_by.clone()?, w.confirmed))
    }

    fn restore_take_state(&self, session: &str, prev: Option<(String, bool)>) {
        match prev {
            Some((d, true)) => self.confirm_taken(session, &d),
            Some((d, false)) => self.set_taken(session, Some(&d)),
            None => self.set_taken(session, None),
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
            let close = w.opened_by.as_deref().is_some_and(|d| {
                removed.iter().any(|r| r == d)
                    || (!w.viewers.contains_key(d) && gone(session, d, None))
            });
            w.viewers.retain(|d, _| !removed.contains(d));
            let detach = before > 0 && w.viewers.is_empty();
            if release {
                w.taken_by = None;
                w.confirmed = false;
            }
            if close {
                w.opened_by = None;
            }
            if detach || release || close {
                out.push((
                    session.clone(),
                    Release {
                        detach,
                        release,
                        close,
                    },
                ));
            }
        }
        m.retain(|_, w| !w.viewers.is_empty() || w.taken_by.is_some() || w.opened_by.is_some());
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
    if r.close {
        let _ = gw
            .server
            .call_as(ACTOR, "browser.session_close", json!({"session": session}))
            .await;
    }
}

/// `browser.attach_screencast` for one device: attach on the server, then count the device.
pub async fn attach(gw: &Gateway, actor: &str, device: &str, session: &str) -> ApiResult {
    let _seq = gw.screencasts.seq.lock().await;
    attach_locked(gw, actor, device, session).await
}

async fn attach_locked(gw: &Gateway, actor: &str, device: &str, session: &str) -> ApiResult {
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

/// `browser.take_over` / `browser.release` for one device. A take-over needs the device to be
/// watching the session, and runs under the same lock as detach and lease expiry: a device whose
/// lease ends while its take-over is in flight can never leave the agent blocked, because the
/// lease cannot end until the take-over is recorded, and then it ends both.
pub async fn control(
    gw: &Gateway,
    actor: &str,
    device: &str,
    session: &str,
    take: bool,
) -> ApiResult {
    let _seq = gw.screencasts.seq.lock().await;
    control_locked(gw, actor, device, session, take).await
}

async fn control_locked(
    gw: &Gateway,
    actor: &str,
    device: &str,
    session: &str,
    take: bool,
) -> ApiResult {
    if take && !gw.screencasts.is_viewer(session, device) {
        return Err(ApiError::new(
            "conflict",
            "watch this session first (browser.attach_screencast)",
        ));
    }
    let method = if take {
        "browser.take_over"
    } else {
        "browser.release"
    };
    // Record the take-over before asking the server: if this request is dropped, or its answer
    // is lost, after the server took over, the lease cleanup still releases it. Input waits for
    // the server's answer (`confirm_taken`).
    let prev = gw.screencasts.take_state(session);
    if take {
        gw.screencasts.set_taken(session, Some(device));
    }
    let r = match gw
        .server
        .call_as(actor, method, json!({"session": session}))
        .await
    {
        Ok(r) => r,
        // No answer: the server may have acted. Keep the record so a lease end releases it.
        Err(e) if e.kind == "unavailable" => return Err(e),
        Err(e) => {
            if take {
                gw.screencasts.restore_take_state(session, prev);
            }
            return Err(e);
        }
    };
    let handle = r.get("session").and_then(|v| v.as_str()).unwrap_or(session);
    if handle != session {
        gw.screencasts.set_taken(session, None);
    }
    if take {
        gw.screencasts.confirm_taken(handle, device);
    } else {
        gw.screencasts.set_taken(handle, None);
    }
    Ok(r)
}

/// The longest a frame poll may wait for a newer frame.
pub const MAX_WAIT_MS: u64 = 15_000;

/// `browser.screencast_frame`: renews the device's lease, then reads the latest frame. With
/// `wait_ms` the host holds the call until a newer frame exists (a long poll): the wait runs in
/// the request's own task, so it blocks neither the device's other requests nor the lease
/// sweep, and it counts as polling at both ends.
pub async fn frame(gw: &Gateway, device: &str, mut params: Value) -> ApiResult {
    let session = params
        .get("session")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !gw.screencasts.touch(&session, device, Instant::now()) {
        return Err(ApiError::new(
            "conflict",
            "attach to this session's screencast first (browser.attach_screencast)",
        ));
    }
    if let Some(w) = params.get("wait_ms").and_then(|v| v.as_u64()) {
        params["wait_ms"] = w.min(MAX_WAIT_MS).into();
    }
    let r = gw.server.call("browser.screencast_frame", params).await;
    gw.screencasts.touch(&session, device, Instant::now());
    r
}

/// `browser.preview_open`: a host session on a preview for this device (06 B3.6). It opens the
/// session as a mobile device, counts the device as its viewer, takes it over and remembers who
/// opened it, so the session closes with the device's lease. `params` carries the server's
/// `browser.session_open` parameters.
pub async fn preview_open(gw: &Gateway, actor: &str, device: &str, params: Value) -> ApiResult {
    let _seq = gw.screencasts.seq.lock().await;
    let mut out = gw
        .server
        .call_as(actor, "browser.session_open", params)
        .await?;
    let Some(handle) = out
        .get("session")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return Err(ApiError::new("internal", "the host returned no session"));
    };
    let steps = async {
        attach_locked(gw, actor, device, &handle).await?;
        gw.screencasts.set_opened(&handle, device);
        control_locked(gw, actor, device, &handle, true).await
    };
    if let Err(e) = steps.await {
        // Nothing may be left behind: drop the record and close the host session.
        let r = gw.screencasts.remove_viewer(&handle, device);
        undo(gw, &handle, Release { close: true, ..r }).await;
        gw.screencasts.set_taken(&handle, None);
        return Err(e);
    }
    out["human_control"] = true.into();
    Ok(out)
}

/// `browser.session_close`: only for a session this device opened with `browser.preview_open`.
pub async fn close_session(gw: &Gateway, actor: &str, device: &str, session: &str) -> ApiResult {
    let _seq = gw.screencasts.seq.lock().await;
    if gw.screencasts.opened_by(session).as_deref() != Some(device) {
        return Err(ApiError::new(
            "forbidden",
            "this device did not open that browser session",
        ));
    }
    let r = gw.screencasts.remove_viewer(session, device);
    undo(gw, session, Release { close: false, ..r }).await;
    // The opener may already be gone from the viewer list: make sure the record ends too.
    gw.screencasts.set_taken(session, None);
    gw.server
        .call_as(actor, "browser.session_close", json!({"session": session}))
        .await
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
        assert!(s.is_viewer("b1", "tablet") && !s.is_viewer("b1", "watch"));
        s.set_taken("b1", Some("phone"));
        assert_eq!(s.taken_by("b1").as_deref(), Some("phone"));
        // A take-over in flight is released with its lease, but carries no input yet.
        assert_eq!(s.controller("b1"), None);
        s.confirm_taken("b1", "phone");
        assert_eq!(s.controller("b1").as_deref(), Some("phone"));
        // The phone leaves: control goes back, but the tablet still watches.
        assert_eq!(
            s.remove_viewer("b1", "phone"),
            Release {
                detach: false,
                release: true,
                close: false
            }
        );
        assert_eq!(s.taken_by("b1"), None);
        // The last viewer leaves: detach.
        assert_eq!(
            s.remove_viewer("b1", "tablet"),
            Release {
                detach: true,
                release: false,
                close: false
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
                    release: true,
                    close: false
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
                    release: true,
                    close: false
                }
            )]
        );
        assert_eq!(s.taken_by("b3"), None);
    }

    #[test]
    fn opened_sessions_are_counted_per_device() {
        let s = Screencasts::default();
        assert_eq!(s.opened_count("phone"), 0);
        s.set_opened("b1", "phone");
        s.set_opened("b2", "phone");
        s.set_opened("b3", "tablet");
        assert_eq!(s.opened_count("phone"), 2);
        assert_eq!(s.opened_count("tablet"), 1);
    }

    #[test]
    fn the_session_a_device_opened_closes_with_its_lease() {
        let s = Screencasts::default();
        let t0 = Instant::now();
        s.add_viewer("b1", "phone", t0);
        s.set_opened("b1", "phone");
        s.set_taken("b1", Some("phone"));
        assert_eq!(s.opened_by("b1").as_deref(), Some("phone"));
        // Other devices watching a session they did not open never close it.
        s.add_viewer("b2", "tablet", t0);
        assert_eq!(
            s.remove_viewer("b2", "tablet"),
            Release {
                detach: true,
                release: false,
                close: false
            }
        );
        // The lease ends: release, detach and close.
        let gone = s.expire(t0 + LEASE + Duration::from_secs(1));
        assert_eq!(
            gone,
            vec![(
                "b1".to_string(),
                Release {
                    detach: true,
                    release: true,
                    close: true
                }
            )]
        );
        assert_eq!(s.opened_by("b1"), None);
        // A disconnect closes it too.
        s.add_viewer("b3", "phone", t0);
        s.set_opened("b3", "phone");
        let gone = s.device_gone("phone");
        assert!(gone.iter().any(|(b, r)| b == "b3" && r.close));
    }
}
