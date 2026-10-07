//! The Vibeke gateway (spec 16 §7): runs next to the server, keeps an outbound control connection
//! to the relay, terminates the end-to-end channel for each device, serves the app API and sends
//! Web Push.

pub mod api;
pub mod cli;
pub mod events;
pub mod handoff;
pub mod local;
pub mod notify;
pub mod pair;
pub mod peer_client;
pub mod peers;
pub mod push;
pub mod relay_client;
pub mod server;
pub mod session;
pub mod state;
pub mod stt;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use vk_e2e::{HostKeys, b64};

use crate::events::Hub;
use crate::server::Server;
use crate::state::{Config, Device, StateDir};

/// Commands to a live device connection.
#[derive(Debug)]
pub enum ConnCmd {
    /// Send an authenticated notification, then close.
    Revoked,
}

pub struct Gateway {
    pub state: StateDir,
    pub cfg: Config,
    pub keys: HostKeys,
    pub host_name: String,
    pub server: Arc<Server>,
    pub hub: Arc<Hub>,
    pub push: push::Sender,
    pub ops: api::Ops,
    devices: RwLock<Vec<Device>>,
    /// Device id → when its foreground lease expires (spec 16 §7.4 `client.visibility`).
    visible: Mutex<HashMap<String, Instant>>,
    live: Mutex<HashMap<String, Vec<mpsc::Sender<ConnCmd>>>>,
    push_throttle: Mutex<HashMap<String, Throttle>>,
    /// Bounds concurrent push sends.
    push_slots: tokio::sync::Semaphore,
    /// Accepted relay connections still before their handshake completes.
    pub dialing: std::sync::atomic::AtomicUsize,
    pub limits: GatewayLimits,
}

#[derive(Debug, Clone)]
pub struct GatewayLimits {
    pub max_connections: usize,
    pub max_inflight: usize,
    pub pair_handshakes_per_min: u32,
}

impl Default for GatewayLimits {
    fn default() -> Self {
        GatewayLimits {
            max_connections: 16,
            max_inflight: 32,
            pair_handshakes_per_min: 4,
        }
    }
}

impl Gateway {
    pub fn new(state: StateDir, server: Arc<Server>) -> Result<Arc<Self>> {
        let cfg = state.config()?;
        let keys = state.host_keys()?;
        let devices = {
            let lock = state.lock()?;
            state.prune_expired_devices(&lock)?.0
        };
        let host_name = cfg.host_name.clone().unwrap_or_else(hostname);
        let push = push::Sender::new(cfg.push_allowed_hosts.clone(), cfg.push_subject.clone())?;
        Ok(Arc::new(Gateway {
            state,
            cfg,
            keys,
            host_name,
            server,
            hub: Arc::new(Hub::default()),
            push,
            ops: api::Ops::default(),
            devices: RwLock::new(devices),
            visible: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            dialing: std::sync::atomic::AtomicUsize::new(0),
            push_throttle: Mutex::new(HashMap::new()),
            push_slots: tokio::sync::Semaphore::new(8),
            limits: GatewayLimits::default(),
        }))
    }

    /// Reload devices from disk (another process — `vibeke-gateway revoke` — may have changed them).
    pub fn reload_devices(&self) -> Result<()> {
        // Read and swap the cache under the registry lock, so a reload can't race a revocation
        // and put a revoked device back in memory.
        // Expired devices leave devices.json here too (the sweep runs every 5 s).
        let lock = self.state.lock()?;
        let (fresh, _) = self.state.prune_expired_devices(&lock)?;
        let gone: Vec<String> = {
            let cur = self.devices.read().unwrap();
            cur.iter()
                .filter(|d| d.expired() || !fresh.iter().any(|f| f.id == d.id))
                .map(|d| d.id.clone())
                .collect()
        };
        *self.devices.write().unwrap() = fresh;
        for id in gone {
            self.disconnect(&id);
        }
        Ok(())
    }

    pub fn devices(&self) -> Vec<Device> {
        self.devices.read().unwrap().clone()
    }

    pub fn device_by_key(&self, public: &[u8; 32]) -> Option<Device> {
        let k = b64::encode(public);
        self.devices
            .read()
            .unwrap()
            .iter()
            .filter(|d| !d.expired())
            .find(|d| d.public == k)
            .cloned()
    }

    pub fn device(&self, id: &str) -> Option<Device> {
        self.devices
            .read()
            .unwrap()
            .iter()
            .find(|d| d.id == id)
            .cloned()
    }

    pub fn add_device(&self, d: Device) -> Result<()> {
        let lock = self.state.lock()?;
        self.add_device_locked(&lock, d)
    }

    /// Add a device while the caller holds the registry lock.
    pub fn add_device_locked(&self, _lock: &state::RegistryLock, d: Device) -> Result<()> {
        let mut all = self.state.devices()?;
        all.retain(|x| x.public != d.public);
        all.push(d);
        self.state.save_devices(&all)?;
        *self.devices.write().unwrap() = all;
        Ok(())
    }

    pub fn update_device(&self, id: &str, f: impl FnOnce(&mut Device)) -> Result<()> {
        // Read-modify-write under the cross-process lock, so an update can never resurrect a
        // device revoked meanwhile (by `vibeke-gateway revoke` or another connection).
        let _lock = self.state.lock()?;
        let mut all = self.state.devices()?;
        let d = all
            .iter_mut()
            .find(|d| d.id == id)
            .ok_or_else(|| anyhow::anyhow!("device not found"))?;
        f(d);
        self.state.save_devices(&all)?;
        *self.devices.write().unwrap() = all;
        Ok(())
    }

    pub async fn revoke(&self, id: &str) -> Result<()> {
        let _lock = self.state.lock()?;
        let mut all = self.state.devices()?;
        let removed: Vec<String> = all
            .iter()
            .filter(|d| d.id == id || d.name == id)
            .map(|d| d.id.clone())
            .collect();
        anyhow::ensure!(!removed.is_empty(), "no such device");
        all.retain(|d| !removed.contains(&d.id));
        self.state.save_devices(&all)?;
        *self.devices.write().unwrap() = all;
        for r in &removed {
            self.state
                .audit(&json!({"ts": state::now_s(), "event": "device.revoked", "device": r}));
            self.disconnect(r);
        }
        Ok(())
    }

    fn disconnect(&self, id: &str) {
        if let Some(conns) = self.live.lock().unwrap().remove(id) {
            for c in conns {
                let _ = c.try_send(ConnCmd::Revoked);
            }
        }
    }

    /// Live device connections plus accepts being dialed (handshakes in progress).
    pub fn live_connections(&self) -> usize {
        let mut live = self.live.lock().unwrap();
        live.values_mut().for_each(|v| v.retain(|c| !c.is_closed()));
        live.values().map(Vec::len).sum::<usize>()
            + self.dialing.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn register_conn(
        &self,
        device: &str,
        tx: mpsc::Sender<ConnCmd>,
    ) -> Result<(), &'static str> {
        let mut live = self.live.lock().unwrap();
        live.values_mut().for_each(|v| v.retain(|c| !c.is_closed()));
        live.retain(|_, v| !v.is_empty());
        if live.values().map(Vec::len).sum::<usize>() >= self.limits.max_connections {
            return Err("too many connections");
        }
        live.entry(device.into()).or_default().push(tx);
        Ok(())
    }

    pub fn set_visible(&self, device: &str, visible: bool) {
        let mut v = self.visible.lock().unwrap();
        if visible {
            v.insert(device.into(), Instant::now() + Duration::from_secs(60));
        } else {
            v.remove(device);
        }
    }

    /// Renew the foreground lease on ping, only if the app said it is visible.
    pub fn touch_visible(&self, device: &str) {
        let mut v = self.visible.lock().unwrap();
        if let Some(t) = v.get_mut(device) {
            *t = Instant::now() + Duration::from_secs(60);
        }
    }

    pub fn is_visible(&self, device: &str) -> bool {
        self.visible
            .lock()
            .unwrap()
            .get(device)
            .is_some_and(|t| *t > Instant::now())
    }

    pub fn dnd(&self) -> bool {
        self.state
            .host_prefs()
            .map(|p| p.dnd_until > state::now_s())
            .unwrap_or(false)
    }

    /// Send a visible push to every subscription of one device. Returns the number delivered.
    pub async fn push_to(&self, device_id: &str, payload: &Value, urgency: &'static str) -> usize {
        let _ = self.reload_devices();
        let Some(d) = self.device(device_id).filter(|d| !d.expired()) else {
            return 0;
        };
        let Some(vapid) = d.vapid_private.as_deref().and_then(|v| b64::decode(v).ok()) else {
            return 0;
        };
        let body = payload.to_string();
        let msg = push::Message {
            payload: body.as_bytes(),
            ttl: 6 * 3600,
            urgency,
            topic: None,
        };
        // Per-device budget and Retry-After cooldown (spec 16 §8.2: ≤ 120 sends/hour per device).
        {
            let mut t = self.push_throttle.lock().unwrap();
            let e = t.entry(device_id.to_string()).or_default();
            let now = Instant::now();
            if e.cooldown_until.is_some_and(|c| c > now) {
                return 0;
            }
            e.sent
                .retain(|s| now.duration_since(*s) < Duration::from_secs(3600));
            if e.sent.len() >= 120 {
                tracing::warn!(device = %d.name, "push budget exhausted for this hour");
                return 0;
            }
            e.sent.push(now);
        }
        let _permit = self.push_slots.acquire().await;
        let mut ok = 0;
        let mut gone = Vec::new();
        let mut failed = false;
        for sub in &d.push {
            match self.push.send(sub, &vapid, &msg).await {
                push::SendOutcome::Ok => ok += 1,
                push::SendOutcome::Gone => gone.push(sub.endpoint.clone()),
                push::SendOutcome::RetryAfter(after) => {
                    let mut t = self.push_throttle.lock().unwrap();
                    t.entry(device_id.to_string()).or_default().cooldown_until =
                        Some(Instant::now() + after.min(Duration::from_secs(3600)));
                    failed = true;
                }
                push::SendOutcome::Failed(e) => {
                    tracing::warn!(device = %d.name, "push failed: {e}");
                    failed = true;
                }
            }
        }
        let _ = self.update_device(device_id, |d| {
            d.push.retain(|s| !gone.contains(&s.endpoint));
            if ok > 0 {
                d.push_failures = 0;
            } else if failed {
                d.push_failures += 1;
                if d.push_failures >= 5 {
                    // Five failures in a row: stop until the app re-subscribes (it shows a banner).
                    tracing::warn!(device = %d.name, "disabling push after 5 consecutive failures");
                    d.push.clear();
                    d.push_failures = 0;
                }
            }
        });
        ok
    }
}

#[derive(Default)]
struct Throttle {
    sent: Vec<Instant>,
    cooldown_until: Option<Instant>,
}

/// This machine's name (used when no `host_name` is configured).
pub fn default_host_name() -> String {
    hostname()
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for its length; gethostname NUL-terminates on success.
    let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if r == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let name = String::from_utf8_lossy(&buf[..end]).to_string();
        return name.trim_end_matches(".local").to_string();
    }
    "host".into()
}

/// Run the gateway until the process is stopped.
pub async fn run(gw: Arc<Gateway>) -> Result<()> {
    let relay = gw.cfg.relay.clone();
    if relay.is_none() && !gw.cfg.local_socket {
        anyhow::bail!(
            "no relay configured: pass --relay (or enable local_socket for a desktop app on this machine)"
        );
    }
    // Bundles from before a restart have no entry any more.
    handoff::sweep(&gw);
    // Open the local socket first: a desktop app can then tell "server down" (calls answer
    // `unavailable`) from "gateway down" (nothing listening).
    if gw.cfg.local_socket {
        let g = gw.clone();
        tokio::spawn(async move {
            if let Err(e) = local::run(g).await {
                tracing::warn!("local transport: {e:#}");
            }
        });
    }
    // The server must grant full scope; wait for it if it isn't running yet (later restarts are
    // fine too: calls reconnect lazily and the event stream resubscribes).
    let mut warned = false;
    loop {
        match gw.server.call("server.status", json!({})).await {
            Ok(_) => break,
            Err(e) if e.kind == "forbidden" => anyhow::bail!("{}", e.message),
            Err(e) => {
                if !warned {
                    tracing::warn!(
                        "waiting for the Vibeke server at {}: {}",
                        gw.server.path().display(),
                        e.message
                    );
                    warned = true;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    tokio::spawn(server::run_events(gw.server.path().clone(), gw.hub.clone()));
    tokio::spawn(notify::run(gw.clone()));
    tokio::spawn({
        let gw = gw.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                gw.state.sweep_pairings();
                let _ = gw.reload_devices();
            }
        }
    });
    match relay {
        Some(relay) => relay_client::run(gw, &relay).await,
        // Local-only (desktop on this machine): nothing else to do.
        None => std::future::pending().await,
    }
}
