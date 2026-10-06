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
    /// `device` (paired normally), `share` (scoped invitation) or `handoff` (may only deliver
    /// handoffs), spec 16 §15.
    #[serde(default = "device_kind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<Limit>,
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
    pub session: Option<String>,
    pub socket: Option<PathBuf>,
    /// VAPID `sub` claim.
    pub push_subject: String,
    pub push_allowed_hosts: Vec<String>,
    pub stt: Option<Stt>,
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
            session: None,
            socket: None,
            push_subject: "mailto:vibeke@localhost".into(),
            push_allowed_hosts: crate::push::DEFAULT_ALLOWED
                .iter()
                .map(|s| s.to_string())
                .collect(),
            stt: None,
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
            },
            confirmed: None,
            confirmed_claim: None,
            share: None,
        };
        s.save_pairing(&p).unwrap();
        let back = s.pairing("abc").unwrap().unwrap();
        assert_eq!(back.status, p.status);
        assert!(s.pairing("../x").is_err());
    }
}
