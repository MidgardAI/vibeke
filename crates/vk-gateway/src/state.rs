//! The gateway state directory (spec 16 §3): keys, devices, pairings and config.
//!
//! Files are 0600 and written atomically (temp + rename). `vibeke-gateway pair` and `run` share
//! pairings through `pairings/<pid>.json`.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use vk_e2e::{HostKeys, b64};

use crate::push::Subscription;

pub fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn default_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("VIBEKE_GATEWAY_DIR") {
        return PathBuf::from(d);
    }
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("vibeke/gateway")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    View,
    Approve,
    Full,
}

impl std::str::FromStr for Scope {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "view" => Scope::View,
            "approve" => Scope::Approve,
            "full" => Scope::Full,
            _ => bail!("scope must be view, approve or full"),
        })
    }
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::View => "view",
            Scope::Approve => "approve",
            Scope::Full => "full",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DevicePrefs {
    /// `full` | `summary` | `minimal`
    #[serde(default = "default_privacy")]
    pub privacy: String,
    #[serde(default = "yes")]
    pub notify_input: bool,
    #[serde(default)]
    pub notify_done: bool,
    /// Notify shortly before an idle agent's prompt cache expires (`notify::cache_ttl`).
    #[serde(default)]
    pub notify_cache_cold: bool,
}

fn default_privacy() -> String {
    "summary".into()
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub platform: String,
    /// X25519 static public key (base64url).
    pub public: String,
    pub scope: Scope,
    pub paired_at: u64,
    /// The device's VAPID private key, given to this host for signing pushes (spec 16 §8.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vapid_private: Option<String>,
    #[serde(default)]
    pub push: Vec<Subscription>,
    #[serde(default)]
    pub prefs: DevicePrefs,
    /// Consecutive failed push sends (reset on success; 5 disables push).
    #[serde(default)]
    pub push_failures: u32,
    /// `device` (paired normally), `share` (scoped invitation) or `peer` (another Vibeke host that
    /// may only deliver handoffs), spec 16 §15. The retired `handoff` kind is pruned on load.
    #[serde(default = "device_kind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<Limit>,
    /// For `peer` devices (another Vibeke host that hands work to this one): whose host it is and
    /// how it introduced itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<PeerInfo>,
    /// The app's service worker closes notifications on a `{kind: "clear"}` push
    /// (`push.subscribe {supports_clear}`); only such devices are sent one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub supports_clear: bool,
    /// The device whose invitation produced this one (a share or peer a paired phone created).
    /// `None` when the owner created it (terminal, TUI). Revoking the creator revokes this too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

/// Who a `peer` device is (spec 16 §15.3).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerInfo {
    /// `self` (one of the owner's own hosts) or `teammate` (redeemed a handoff invitation).
    pub owner: String,
    /// The sending host's display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_name: Option<String>,
    /// The git identity the sender chose to show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<GitUser>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitUser {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// A host this gateway can hand work to (spec 16 §15.3), kept in `peers.json`. Holds the private
/// key this host authenticates with there.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRecord {
    pub id: String,
    /// The other host's display name.
    pub name: String,
    /// Relay WebSocket base, or `local:<socket>` for a gateway on this machine.
    pub relay: String,
    /// The other host's id on the relay.
    pub host: String,
    /// Its Noise static public key (base64url), pinned at pairing.
    pub host_key: String,
    /// Our X25519 private key for that host (base64url).
    pub device_key: String,
    /// Our device id on that host.
    #[serde(default)]
    pub device_id: String,
    /// `self` or `teammate`.
    pub owner: String,
    pub added_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Relay admission ticket the other host signed for us (spec 16 §6.6), sent as `&ticket=`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_exp: Option<u64>,
    /// The device that redeemed this invitation through the app API. `None` when the owner did.
    /// Revoking that device forgets this peer too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

impl PeerRecord {
    pub fn expired(&self) -> bool {
        self.expires_at.is_some_and(|t| t <= now_s())
    }

    /// The record without its private key, for listings.
    pub fn public_json(&self) -> serde_json::Value {
        serde_json::json!({"id": self.id, "name": self.name, "relay": self.relay, "host": self.host,
                           "device_id": self.device_id, "owner": self.owner, "added_at": self.added_at,
                           "expires_at": self.expires_at, "expired": self.expired()})
    }
}

fn device_kind() -> String {
    "device".into()
}

/// What a share device may see (spec 16 §15.1).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Limit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

/// Invitation details stored with a pending share/handoff pairing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShareSpec {
    pub kind: String,
    pub ttl_s: u64,
    /// Absolute expiry fixed when the invitation is created (the link shows the same time).
    #[serde(default)]
    pub until: u64,
    #[serde(default)]
    pub limit: Option<Limit>,
    #[serde(default)]
    pub label: Option<String>,
    /// `peer` invitations: `self` for the owner's own hosts. Absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The device that created the invitation. `None` when the owner did (terminal, TUI).
    /// Revoking that device cancels the invitation and revokes the device it produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

impl ShareSpec {
    /// When the resulting device stops working; `None` for a peer invitation between the owner's
    /// own hosts (`ttl_s` and `until` both 0).
    pub fn device_expiry(&self) -> Option<u64> {
        match (self.until, self.ttl_s) {
            (0, 0) => None,
            (0, ttl) => Some(now_s() + ttl),
            (until, _) => Some(until),
        }
    }
}

impl Device {
    pub fn expired(&self) -> bool {
        self.expires_at.is_some_and(|t| t <= now_s())
    }

    pub fn fingerprint(&self) -> String {
        b64::decode(&self.public)
            .map(|k| vk_e2e::keys::fingerprint(&k))
            .unwrap_or_default()
    }
}

/// What revoking some devices takes with it: everything those devices created, transitively (a
/// phone's shares and peer invitations, and whatever a device they produced created in turn).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Cascade {
    /// Device ids to revoke: the roots that exist, then what they created.
    pub devices: Vec<String>,
    /// Of those, the devices beyond the roots, by kind (`share`, `peer`, …).
    pub created: Vec<(String, String)>,
    /// Pending or claimed invitations (pairing ids) a revoked device created.
    pub invitations: Vec<String>,
    /// Peer records (hosts this one hands work to) a revoked device redeemed.
    pub peers: Vec<String>,
}

impl Cascade {
    /// Compute the cascade of revoking `roots` (ids or names) without changing anything.
    pub fn of(
        roots: &[String],
        devices: &[Device],
        pairings: &[Pairing],
        peers: &[PeerRecord],
    ) -> Cascade {
        let mut c = Cascade::default();
        for d in devices {
            if roots.iter().any(|r| *r == d.id || *r == d.name) && !c.devices.contains(&d.id) {
                c.devices.push(d.id.clone());
            }
        }
        if c.devices.is_empty() {
            return c;
        }
        // A fixed point: each round adds the devices created by one already in the set.
        loop {
            let more: Vec<&Device> = devices
                .iter()
                .filter(|d| !c.devices.contains(&d.id))
                .filter(|d| d.created_by.as_ref().is_some_and(|b| c.devices.contains(b)))
                .collect();
            if more.is_empty() {
                break;
            }
            for d in more {
                c.devices.push(d.id.clone());
                c.created.push((d.id.clone(), d.kind.clone()));
            }
        }
        let by_revoked = |b: &Option<String>| b.as_ref().is_some_and(|b| c.devices.contains(b));
        c.invitations = pairings
            .iter()
            .filter(|p| !matches!(p.status, PairingStatus::Done { .. }))
            .filter(|p| by_revoked(&p.share.as_ref().and_then(|s| s.created_by.clone())))
            .map(|p| p.pid.clone())
            .collect();
        c.peers = peers
            .iter()
            .filter(|p| by_revoked(&p.created_by))
            .map(|p| p.id.clone())
            .collect();
        c
    }

    /// Counts for the API: `{shares, hosts, invitations, peers}` beyond the roots.
    pub fn also_json(&self) -> serde_json::Value {
        let shares = self.created.iter().filter(|(_, k)| k != "peer").count();
        let hosts = self.created.len() - shares;
        serde_json::json!({"shares": shares, "hosts": hosts, "invitations": self.invitations.len(), "peers": self.peers.len()})
    }

    /// "This also removes 2 shares and 1 invitation." or `None` when nothing else goes.
    pub fn summary(&self) -> Option<String> {
        also_text(&self.also_json())
    }
}

/// The sentence for a [`Cascade::also_json`] object (also used by clients that only have the
/// counts): "This also removes 2 shares, 1 linked host and 1 invitation."
pub fn also_text(also: &serde_json::Value) -> Option<String> {
    let n = |k: &str| also.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let hosts = n("hosts") + n("peers");
    let parts: Vec<String> = [
        (n("shares"), "share", "shares"),
        (hosts, "linked host", "linked hosts"),
        (n("invitations"), "invitation", "invitations"),
    ]
    .into_iter()
    .filter(|(c, _, _)| *c > 0)
    .map(|(c, one, many)| format!("{c} {}", if c == 1 { one } else { many }))
    .collect();
    let list = match parts.as_slice() {
        [] => return None,
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    };
    Some(format!("This also removes {list}."))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PairingStatus {
    Pending,
    Claimed {
        /// Unique per claim: the operator's answer is bound to it (spec 16 §4.3).
        claim_id: String,
        /// Full X25519 public key of the claiming device (base64url).
        device_public: String,
        fingerprint: String,
        device_name: String,
        platform: String,
        /// The pairing's check code (`vk_e2e::keys::check_code` of the handshake hash): the
        /// operator compares it with the code the device shows. Empty in older pairing files.
        #[serde(default)]
        check: String,
    },
    Done {
        device_id: String,
    },
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pairing {
    pub pid: String,
    pub psk: String,
    pub exp: u64,
    pub scope: Scope,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub no_confirm: bool,
    #[serde(flatten)]
    pub status: PairingStatus,
    /// Written by `vibeke-gateway pair` after the operator answers.
    #[serde(default)]
    pub confirmed: Option<bool>,
    /// The `claim_id` the operator was shown; a confirmation for another claim is ignored.
    #[serde(default)]
    pub confirmed_claim: Option<String>,
    #[serde(default)]
    pub share: Option<ShareSpec>,
    /// Unix seconds the pairing was created (0 for pairings from older gateways).
    #[serde(default)]
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Relay base URL, e.g. `wss://relay.example.com`.
    pub relay: Option<String>,
    /// Origin serving the web app (pairing links point here; spec 16 §9.4).
    pub app_url: Option<String>,
    pub host_name: Option<String>,
    /// Token for private relays (`--host-token` on the relay).
    pub relay_token: Option<String>,
    /// Control plane for the hosted relay's accounts (spec 16 §6.6). Defaults to the relay origin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_url: Option<String>,
    pub session: Option<String>,
    pub socket: Option<PathBuf>,
    /// Whether the server starts the gateway for you. `None` = on when a relay is saved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autostart: Option<bool>,
    /// VAPID `sub` claim.
    pub push_subject: String,
    pub push_allowed_hosts: Vec<String>,
    pub stt: Option<Stt>,
    /// Serve the channel on `<state dir>/gateway.sock` for desktop apps on this machine.
    pub local_socket: bool,
}

impl Config {
    /// The gateway is set up for autostart. `local_socket` defaults to true, so it never counts.
    pub fn autostart_enabled(&self) -> bool {
        self.autostart == Some(true) || (self.autostart.is_none() && self.relay.is_some())
    }

    pub fn session_name(&self) -> &str {
        self.session.as_deref().unwrap_or("default")
    }
}

/// True when the gateway in `dir` is set up for autostart for `session` (the enabled rule, and the
/// `gateway.toml` session — default "default" — equals `session`). Missing dir/file → false.
pub fn autostart_for_session(dir: &Path, session: &str) -> bool {
    let Ok(text) = fs::read_to_string(dir.join("gateway.toml")) else {
        return false;
    };
    let Ok(cfg) = toml::from_str::<Config>(&text) else {
        return false;
    };
    cfg.autostart_enabled() && cfg.session_name() == session
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stt {
    /// argv; `{file}` is replaced with the audio file path. Must print the transcript on stdout.
    pub command: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            relay: None,
            app_url: None,
            host_name: None,
            relay_token: None,
            account_url: None,
            session: None,
            socket: None,
            autostart: None,
            push_subject: "mailto:vibeke@localhost".into(),
            push_allowed_hosts: crate::push::DEFAULT_ALLOWED
                .iter()
                .map(|s| s.to_string())
                .collect(),
            stt: None,
            local_socket: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostPrefs {
    /// Do-not-disturb until (unix seconds), host-wide.
    #[serde(default)]
    pub dnd_until: u64,
}

/// Held while the registry is being changed; released on drop (closing the descriptor).
pub struct RegistryLock {
    _f: fs::File,
}

pub struct StateDir {
    pub dir: PathBuf,
}

impl StateDir {
    pub fn open(dir: PathBuf) -> Result<Self> {
        if !dir.exists() {
            fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        }
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        check_owned(&dir)?;
        fs::create_dir_all(dir.join("pairings"))?;
        fs::set_permissions(dir.join("pairings"), fs::Permissions::from_mode(0o700))?;
        Ok(StateDir { dir })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn host_keys(&self) -> Result<HostKeys> {
        let p = self.path("host.json");
        if p.exists() {
            check_owned(&p)?;
            return serde_json::from_slice(&fs::read(&p)?).context("host.json");
        }
        let k = HostKeys::generate();
        write_json(&p, &k)?;
        Ok(k)
    }

    pub fn config(&self) -> Result<Config> {
        let p = self.path("gateway.toml");
        if !p.exists() {
            return Ok(Config::default());
        }
        toml::from_str(&fs::read_to_string(&p)?).context("gateway.toml")
    }

    pub fn save_config(&self, c: &Config) -> Result<()> {
        write_atomic(
            &self.path("gateway.toml"),
            toml::to_string_pretty(c)?.as_bytes(),
        )
    }

    /// Exclusive, cross-process lock for read-modify-write of the device registry and pairings.
    pub fn lock(&self) -> Result<RegistryLock> {
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(self.path("registry.lock"))?;
        // SAFETY: flock on an owned, open descriptor.
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&f), libc::LOCK_EX) } != 0 {
            anyhow::bail!("lock registry: {}", std::io::Error::last_os_error());
        }
        Ok(RegistryLock { _f: f })
    }

    pub fn devices(&self) -> Result<Vec<Device>> {
        read_json_or_default(&self.path("devices.json"))
    }

    pub fn save_devices(&self, d: &[Device]) -> Result<()> {
        write_json(&self.path("devices.json"), &d)
    }

    /// Remove expired share/peer devices from `devices.json` (they are refused at the handshake
    /// already; this keeps the registry from growing). The caller holds the registry lock.
    /// Returns the remaining devices and the ids removed.
    pub fn prune_expired_devices(
        &self,
        _lock: &RegistryLock,
    ) -> Result<(Vec<Device>, Vec<String>)> {
        let mut all = self.devices()?;
        let gone: Vec<String> = all
            .iter()
            .filter(|d| d.expired())
            .map(|d| d.id.clone())
            .collect();
        if !gone.is_empty() {
            all.retain(|d| !gone.contains(&d.id));
            self.save_devices(&all)?;
            for id in &gone {
                self.audit(
                    &serde_json::json!({"ts": now_s(), "event": "device.expired", "device": id}),
                );
            }
        }
        Ok((all, gone))
    }

    /// Revoke the devices `ids` and everything they created, transitively ([`Cascade`]): the
    /// devices their invitations produced, their pending invitations and the peers they
    /// redeemed. The caller holds the registry lock. Returns what was removed and the devices
    /// that remain. Nothing is written when `ids` names no device.
    pub fn revoke_cascade(
        &self,
        _lock: &RegistryLock,
        ids: &[String],
        by: &str,
    ) -> Result<(Cascade, Vec<Device>)> {
        let mut devices = self.devices()?;
        let pairings = self.pairings();
        let mut peers = self.peers()?;
        let c = Cascade::of(ids, &devices, &pairings, &peers);
        if c.devices.is_empty() {
            return Ok((c, devices));
        }
        devices.retain(|d| !c.devices.contains(&d.id));
        self.save_devices(&devices)?;
        if !c.peers.is_empty() {
            peers.retain(|p| !c.peers.contains(&p.id));
            self.save_peers(&peers)?;
        }
        for pid in &c.invitations {
            let _ = self.remove_pairing(pid);
        }
        for id in &c.devices {
            let cause = (!ids.contains(id)).then_some("creator_revoked");
            self.audit(&serde_json::json!({"ts": now_s(), "event": "device.revoked", "device": id, "by": by, "cause": cause}));
        }
        for pid in &c.invitations {
            self.audit(&serde_json::json!({"ts": now_s(), "event": "invitation.cancelled", "pid": pid, "by": by, "cause": "creator_revoked"}));
        }
        for id in &c.peers {
            self.audit(&serde_json::json!({"ts": now_s(), "event": "peer.removed", "peer": id, "by": by, "cause": "creator_revoked"}));
        }
        Ok((c, devices))
    }

    /// Hosts this gateway can hand work to (`peers.json`, 0600: it holds private keys).
    pub fn peers(&self) -> Result<Vec<PeerRecord>> {
        let p = self.path("peers.json");
        if p.exists() {
            check_owned(&p)?;
        }
        read_json_or_default(&p)
    }

    pub fn save_peers(&self, peers: &[PeerRecord]) -> Result<()> {
        write_json(&self.path("peers.json"), &peers)
    }

    /// Every stored pairing (pending, claimed or done) that still parses, oldest first.
    pub fn pairings(&self) -> Vec<Pairing> {
        let Ok(rd) = fs::read_dir(self.dir.join("pairings")) else {
            return Vec::new();
        };
        let mut out: Vec<Pairing> = rd
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| serde_json::from_slice(&fs::read(e.path()).ok()?).ok())
            .collect();
        out.sort_by_key(|p| (p.created_at, p.exp));
        out
    }

    pub fn host_prefs(&self) -> Result<HostPrefs> {
        read_json_or_default(&self.path("prefs.json"))
    }

    pub fn save_host_prefs(&self, p: &HostPrefs) -> Result<()> {
        write_json(&self.path("prefs.json"), p)
    }

    fn pairing_path(&self, pid: &str) -> Result<PathBuf> {
        if pid.is_empty()
            || pid.len() > 64
            || !pid
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            bail!("bad pairing id");
        }
        Ok(self.dir.join("pairings").join(format!("{pid}.json")))
    }

    pub fn pairing(&self, pid: &str) -> Result<Option<Pairing>> {
        let p = self.pairing_path(pid)?;
        if !p.exists() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_slice(&fs::read(p)?)?))
    }

    pub fn save_pairing(&self, pairing: &Pairing) -> Result<()> {
        write_json(&self.pairing_path(&pairing.pid)?, pairing)
    }

    pub fn remove_pairing(&self, pid: &str) -> Result<()> {
        let p = self.pairing_path(pid)?;
        if p.exists() {
            fs::remove_file(p)?;
        }
        Ok(())
    }

    /// Delete expired pairings.
    pub fn sweep_pairings(&self) {
        let Ok(rd) = fs::read_dir(self.dir.join("pairings")) else {
            return;
        };
        for e in rd.flatten() {
            if let Ok(p) =
                serde_json::from_slice::<Pairing>(&fs::read(e.path()).unwrap_or_default())
                && p.exp + 60 < now_s()
            {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    pub fn audit(&self, entry: &serde_json::Value) {
        let p = self.path("audit.log");
        let line = format!("{entry}\n");
        let r = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&p)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = r {
            tracing::warn!("audit log: {e}");
        }
    }
}

/// `status.json`: what `gateway run` is doing right now (read by the server and the CLI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusFile {
    pub pid: u32,
    /// `connecting` | `online` | `offline` | `local_only` | `login_required` (the relay needs an
    /// account: `vibeke login`, spec 16 §6.6)
    pub state: String,
    pub relay: Option<String>,
    pub devices: u32,
    /// Unix ms of the last state change.
    pub since_ms: u64,
    pub last_error: Option<String>,
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn status_path(dir: &Path) -> PathBuf {
    dir.join("status.json")
}

pub fn read_status(dir: &Path) -> Option<StatusFile> {
    serde_json::from_slice(&fs::read(status_path(dir)).ok()?).ok()
}

/// Publishes [`StatusFile`] on every state change. Inert until `enable` (tests, one-shot commands).
pub struct StatusWriter {
    dir: PathBuf,
    cur: std::sync::Mutex<Option<StatusFile>>,
}

impl StatusWriter {
    pub fn new(dir: PathBuf) -> Self {
        StatusWriter {
            dir,
            cur: std::sync::Mutex::new(None),
        }
    }

    pub fn enable(&self, state: &str, relay: Option<String>, devices: usize) {
        let mut cur = self.cur.lock().unwrap();
        *cur = Some(StatusFile {
            pid: std::process::id(),
            state: state.into(),
            relay,
            devices: devices as u32,
            since_ms: now_ms(),
            last_error: None,
        });
        self.flush(cur.as_ref());
    }

    /// Change state. `error` replaces `last_error`; reaching `online` clears it.
    pub fn set_state(&self, state: &str, error: Option<String>) {
        let mut cur = self.cur.lock().unwrap();
        let Some(s) = cur.as_mut() else { return };
        if s.state == state && error.is_none() {
            return;
        }
        s.state = state.into();
        s.since_ms = now_ms();
        if error.is_some() || state == "online" {
            s.last_error = error;
        }
        self.flush(cur.as_ref());
    }

    pub fn set_devices(&self, devices: usize) {
        let mut cur = self.cur.lock().unwrap();
        let Some(s) = cur.as_mut() else { return };
        if s.devices == devices as u32 {
            return;
        }
        s.devices = devices as u32;
        self.flush(cur.as_ref());
    }

    fn flush(&self, s: Option<&StatusFile>) {
        if let Some(s) = s
            && let Err(e) = write_json(&status_path(&self.dir), s)
        {
            tracing::warn!("status.json: {e}");
        }
    }

    /// Clean exit: remove the file.
    pub fn clear(&self) {
        *self.cur.lock().unwrap() = None;
        let _ = fs::remove_file(status_path(&self.dir));
    }
}

/// Exclusive flock on `run.lock`, held for the life of `gateway run`.
pub struct RunLock {
    _f: fs::File,
}

impl RunLock {
    /// `Ok(Ok(lock))` when acquired (our pid is written into the file), `Ok(Err(pid))` when another
    /// gateway holds it (`pid` 0 if unreadable).
    pub fn acquire(dir: &Path) -> Result<std::result::Result<RunLock, u32>> {
        Self::acquire_inner(dir, true)
    }

    fn acquire_inner(dir: &Path, write_pid: bool) -> Result<std::result::Result<RunLock, u32>> {
        let p = dir.join("run.lock");
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&p)?;
        // SAFETY: flock on an owned, open descriptor.
        let r = unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&f),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        if r != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                let pid = fs::read_to_string(&p)
                    .ok()
                    .and_then(|t| t.trim().parse().ok())
                    .unwrap_or(0);
                return Ok(Err(pid));
            }
            bail!("lock {}: {err}", p.display());
        }
        if write_pid {
            f.set_len(0)?;
            write!(f, "{}", std::process::id())?;
            f.sync_all()?;
        }
        Ok(Ok(RunLock { _f: f }))
    }

    /// Pid of the gateway running from `dir`, if any (probes the lock; never keeps it).
    pub fn holder(dir: &Path) -> Option<u32> {
        match Self::acquire_inner(dir, false) {
            Ok(Err(pid)) => Some(pid),
            _ => None,
        }
    }
}

fn check_owned(p: &Path) -> Result<()> {
    let md = fs::symlink_metadata(p)?;
    if md.file_type().is_symlink() {
        bail!("{} is a symlink", p.display());
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    if md.uid() != uid {
        bail!("{} is not owned by the current user", p.display());
    }
    if md.mode() & 0o022 != 0 {
        bail!("{} is group/world-writable", p.display());
    }
    Ok(())
}

fn read_json_or_default<T: serde::de::DeserializeOwned + Default>(p: &Path) -> Result<T> {
    if !p.exists() {
        return Ok(T::default());
    }
    serde_json::from_slice(&fs::read(p)?).with_context(|| p.display().to_string())
}

pub fn write_json<T: Serialize + ?Sized>(p: &Path, v: &T) -> Result<()> {
    write_atomic(p, &serde_json::to_vec_pretty(v)?)
}

pub fn write_atomic(p: &Path, bytes: &[u8]) -> Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = p.with_extension(format!(
        "tmp{}-{n}-{:08x}",
        std::process::id(),
        rand::random::<u32>()
    ));
    {
        let mut f = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, p)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_persist_and_pairing_roundtrip() {
        let t = tempfile::tempdir().unwrap();
        let s = StateDir::open(t.path().join("gw")).unwrap();
        let k1 = s.host_keys().unwrap();
        let k2 = s.host_keys().unwrap();
        assert_eq!(k1.host_id(), k2.host_id());
        let mode = fs::metadata(t.path().join("gw/host.json")).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let p = Pairing {
            pid: "abc".into(),
            psk: "x".into(),
            exp: now_s() + 60,
            scope: Scope::Full,
            name: None,
            no_confirm: false,
            status: PairingStatus::Claimed {
                claim_id: "c1".into(),
                device_public: "k".into(),
                fingerprint: "ab-cd".into(),
                device_name: "n".into(),
                platform: "ios".into(),
                check: "ABCD-EFGH-IJKL".into(),
            },
            confirmed: None,
            confirmed_claim: None,
            share: None,
            created_at: now_s(),
        };
        s.save_pairing(&p).unwrap();
        let back = s.pairing("abc").unwrap().unwrap();
        assert_eq!(back.status, p.status);
        assert!(s.pairing("../x").is_err());
        assert_eq!(s.pairings().len(), 1);
    }

    fn dev(id: &str, kind: &str, by: Option<&str>) -> Device {
        let mut d: Device = serde_json::from_value(serde_json::json!({
            "id": id, "name": format!("n-{id}"), "public": format!("k-{id}"), "scope": "full",
            "paired_at": 0, "kind": kind,
        }))
        .unwrap();
        d.created_by = by.map(str::to_string);
        d
    }

    fn invitation(pid: &str, by: Option<&str>, status: PairingStatus) -> Pairing {
        Pairing {
            pid: pid.into(),
            psk: "x".into(),
            exp: now_s() + 600,
            scope: Scope::View,
            name: None,
            no_confirm: true,
            status,
            confirmed: None,
            confirmed_claim: None,
            share: Some(ShareSpec {
                kind: "share".into(),
                ttl_s: 3600,
                until: now_s() + 3600,
                limit: None,
                label: None,
                owner: None,
                created_by: by.map(str::to_string),
            }),
            created_at: now_s(),
        }
    }

    fn peer_rec(id: &str, by: Option<&str>) -> PeerRecord {
        PeerRecord {
            id: id.into(),
            name: id.into(),
            relay: "wss://r".into(),
            host: format!("h-{id}"),
            host_key: "hk".into(),
            device_key: "k".into(),
            device_id: "d".into(),
            owner: "self".into(),
            added_at: 0,
            expires_at: None,
            ticket: None,
            ticket_exp: None,
            created_by: by.map(str::to_string),
        }
    }

    #[test]
    fn revoking_a_device_revokes_what_it_created() {
        let devices = vec![
            dev("phone", "device", None),
            dev("tablet", "device", None),
            dev("s1", "share", Some("phone")),
            dev("s2", "share", Some("phone")),
            dev("h1", "peer", Some("phone")),
            dev("s3", "share", Some("tablet")),
            dev("owner-share", "share", None),
        ];
        let pairings = vec![
            invitation("i1", Some("phone"), PairingStatus::Pending),
            invitation("i2", Some("tablet"), PairingStatus::Pending),
            invitation(
                "used",
                Some("phone"),
                PairingStatus::Done {
                    device_id: "s1".into(),
                },
            ),
        ];
        let peers = vec![peer_rec("p1", Some("phone")), peer_rec("p2", None)];
        let c = Cascade::of(&["phone".into()], &devices, &pairings, &peers);
        assert_eq!(c.devices, ["phone", "s1", "s2", "h1"]);
        assert_eq!(c.invitations, ["i1"]);
        assert_eq!(c.peers, ["p1"]);
        assert_eq!(
            c.also_json(),
            serde_json::json!({"shares": 2, "hosts": 1, "invitations": 1, "peers": 1})
        );
        assert_eq!(
            c.summary().unwrap(),
            "This also removes 2 shares, 2 linked hosts and 1 invitation."
        );
        // By name too; a device that created nothing takes nothing with it.
        let c = Cascade::of(&["n-owner-share".into()], &devices, &pairings, &peers);
        assert_eq!(c.devices, ["owner-share"]);
        assert_eq!(c.summary(), None);
        // Unknown ids revoke nothing, not even what an unknown id "created".
        assert_eq!(
            Cascade::of(&["ghost".into()], &devices, &pairings, &peers),
            Cascade::default()
        );
    }

    #[test]
    fn revoke_cascade_writes_the_registry() {
        let t = tempfile::tempdir().unwrap();
        let s = StateDir::open(t.path().join("gw")).unwrap();
        s.save_devices(&[
            dev("phone", "device", None),
            dev("s1", "share", Some("phone")),
            dev("keep", "device", None),
        ])
        .unwrap();
        s.save_pairing(&invitation("i1", Some("phone"), PairingStatus::Pending))
            .unwrap();
        s.save_pairing(&invitation("i2", None, PairingStatus::Pending))
            .unwrap();
        s.save_peers(&[peer_rec("p1", Some("phone")), peer_rec("p2", None)])
            .unwrap();
        let lock = s.lock().unwrap();
        let (c, left) = s.revoke_cascade(&lock, &["phone".into()], "cli").unwrap();
        assert_eq!(c.devices, ["phone", "s1"]);
        assert_eq!(
            left.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["keep"]
        );
        assert_eq!(s.devices().unwrap().len(), 1);
        assert!(s.pairing("i1").unwrap().is_none());
        assert!(s.pairing("i2").unwrap().is_some());
        assert_eq!(s.peers().unwrap().len(), 1);
        assert_eq!(s.peers().unwrap()[0].id, "p2");
        // Nothing matched: nothing written.
        let (c, left) = s.revoke_cascade(&lock, &["ghost".into()], "cli").unwrap();
        assert!(c.devices.is_empty());
        assert_eq!(left.len(), 1);
    }

    #[test]
    fn also_text_lists_counts() {
        assert_eq!(also_text(&serde_json::json!({})), None);
        assert_eq!(
            also_text(&serde_json::json!({"shares": 1})).unwrap(),
            "This also removes 1 share."
        );
        assert_eq!(
            also_text(&serde_json::json!({"shares": 2, "invitations": 1})).unwrap(),
            "This also removes 2 shares and 1 invitation."
        );
    }

    fn write_cfg(dir: &Path, text: &str) {
        fs::write(dir.join("gateway.toml"), text).unwrap();
    }

    #[test]
    fn autostart_rule() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        // Missing file and missing dir.
        assert!(!autostart_for_session(d, "default"));
        assert!(!autostart_for_session(&d.join("nope"), "default"));
        // local_socket defaults to true but never counts.
        write_cfg(d, "");
        assert!(!autostart_for_session(d, "default"));
        write_cfg(d, "local_socket = true\n");
        assert!(!autostart_for_session(d, "default"));
        // A saved relay enables it; an explicit false overrides.
        write_cfg(d, "relay = \"wss://r.example\"\n");
        assert!(autostart_for_session(d, "default"));
        write_cfg(d, "relay = \"wss://r.example\"\nautostart = false\n");
        assert!(!autostart_for_session(d, "default"));
        // Explicit true without a relay.
        write_cfg(d, "autostart = true\n");
        assert!(autostart_for_session(d, "default"));
        // Session mismatch.
        write_cfg(d, "autostart = true\nsession = \"work\"\n");
        assert!(autostart_for_session(d, "work"));
        assert!(!autostart_for_session(d, "default"));
        // Garbage never errors.
        write_cfg(d, "not toml {{{");
        assert!(!autostart_for_session(d, "default"));
    }

    #[test]
    fn config_autostart_roundtrip() {
        let t = tempfile::tempdir().unwrap();
        let s = StateDir::open(t.path().join("gw")).unwrap();
        let mut c = s.config().unwrap();
        assert_eq!(c.autostart, None);
        c.autostart = Some(true);
        s.save_config(&c).unwrap();
        assert_eq!(s.config().unwrap().autostart, Some(true));
    }

    #[test]
    fn status_file_roundtrip() {
        let t = tempfile::tempdir().unwrap();
        let w = StatusWriter::new(t.path().to_path_buf());
        w.set_state("online", None);
        assert!(read_status(t.path()).is_none(), "inert until enabled");
        w.enable("connecting", Some("wss://r.example".into()), 2);
        let s = read_status(t.path()).unwrap();
        assert_eq!(s.pid, std::process::id());
        assert_eq!(s.state, "connecting");
        assert_eq!(s.devices, 2);
        w.set_state("offline", Some("boom".into()));
        let s = read_status(t.path()).unwrap();
        assert_eq!(
            (s.state.as_str(), s.last_error.as_deref()),
            ("offline", Some("boom"))
        );
        w.set_state("online", None);
        assert_eq!(read_status(t.path()).unwrap().last_error, None);
        w.clear();
        assert!(read_status(t.path()).is_none());
    }

    #[test]
    fn run_lock_is_exclusive() {
        let t = tempfile::tempdir().unwrap();
        let first = RunLock::acquire(t.path()).unwrap().ok().unwrap();
        // flock is per open file description, so a second open in-process is refused.
        let second = RunLock::acquire(t.path()).unwrap();
        assert_eq!(second.err(), Some(std::process::id()));
        drop(first);
        assert!(RunLock::acquire(t.path()).unwrap().is_ok());
    }

    fn device(id: &str, expires_at: Option<u64>) -> Device {
        Device {
            id: id.into(),
            name: id.into(),
            platform: String::new(),
            public: format!("k-{id}"),
            scope: Scope::Full,
            paired_at: 0,
            vapid_private: None,
            push: vec![],
            prefs: Default::default(),
            push_failures: 0,
            kind: "share".into(),
            expires_at,
            limit: None,
            peer: None,
            supports_clear: false,
            created_by: None,
        }
    }

    #[test]
    fn expired_devices_are_pruned() {
        let t = tempfile::tempdir().unwrap();
        let s = StateDir::open(t.path().join("gw")).unwrap();
        s.save_devices(&[
            device("old", Some(now_s() - 10)),
            device("live", Some(now_s() + 3600)),
            device("forever", None),
        ])
        .unwrap();
        let lock = s.lock().unwrap();
        let (left, gone) = s.prune_expired_devices(&lock).unwrap();
        assert_eq!(gone, ["old"]);
        let ids: Vec<_> = left.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["live", "forever"]);
        assert_eq!(s.devices().unwrap().len(), 2);
        let audit = fs::read_to_string(t.path().join("gw/audit.log")).unwrap();
        assert!(audit.contains("device.expired") && audit.contains("\"old\""));
    }

    #[test]
    fn share_spec_expiry() {
        let mut sp = ShareSpec {
            kind: "peer".into(),
            ttl_s: 0,
            until: 0,
            limit: None,
            label: None,
            owner: Some("self".into()),
            created_by: None,
        };
        assert_eq!(sp.device_expiry(), None);
        sp.until = 5;
        assert_eq!(sp.device_expiry(), Some(5));
    }

    #[test]
    fn peers_file_is_private() {
        let t = tempfile::tempdir().unwrap();
        let s = StateDir::open(t.path().join("gw")).unwrap();
        assert!(s.peers().unwrap().is_empty());
        let r = PeerRecord {
            id: "p1".into(),
            name: "devbox".into(),
            relay: "wss://r".into(),
            host: "h".into(),
            host_key: "hk".into(),
            device_key: "secret".into(),
            device_id: "d".into(),
            owner: "self".into(),
            added_at: 1,
            expires_at: None,
            ticket: Some("tk".into()),
            ticket_exp: None,
            created_by: None,
        };
        s.save_peers(std::slice::from_ref(&r)).unwrap();
        let mode = fs::metadata(t.path().join("gw/peers.json")).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(s.peers().unwrap()[0].device_key, "secret");
        assert!(!r.public_json().to_string().contains("secret"));
        assert!(!r.public_json().to_string().contains("tk"));
    }
}
