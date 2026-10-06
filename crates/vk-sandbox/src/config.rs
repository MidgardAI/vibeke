//! `[isolation]` user config (13 §3). The section is owned by this crate; `vk-config` keeps it
//! verbatim in `Config::extra`.
//!
//! ```toml
//! [isolation]
//! yolo_default = "sandbox"        # level applied by --yolo without --isolate
//! network = "dev"                 # default network profile for contained tasks
//! confirm_host_yolo = true        # one-time confirmation per workspace for --yolo --isolate host
//! suggest_sandbox_for_yolo = false  # nudge: relaunch a user-typed host yolo run in a sandbox
//! idle_suspend = "30m"            # pause idle container boxes ("off" disables)
//! resource_pressure = 0.9         # sandbox.resource_pressure above this share of a box limit
//! resource_poll = "30s"           # how often box usage is sampled
//!
//! [isolation.sandbox]
//! read = ["~/.zshrc"]             # extra home paths readable inside (read-only)
//! write = []                      # extra read-write paths (e.g. shared caches)
//! allow_domains = ["example.com"] # added to every profile except none
//! deny_domains = []
//! local_ports = [5432]            # loopback services reachable through the proxy
//! ports = [443, 80]               # ports allowlisted domains may use (default 80/443)
//! allow_private = false
//! broker = true                   # per-pane broker socket (hooks inside the box)
//! sni_check = true                # TLS SNI / Host cross-check against the checked host
//! global_approvals = true         # offer "allow always" (every task) on egress Interactions
//!
//! [isolation.container]
//! image = "ghcr.io/acme/dev:node22"  # default image (repo/devcontainer/--image override it)
//! runtime = "docker"              # docker | orbstack | podman | apple-container | /abs/cli
//! code = "clone"                  # clone (default) | worktree (bind-mount the host worktree)
//! cpus = "4"
//! memory = "8g"
//! pids = 1024
//! vibeke_linux = "~/bin/vibeke-linux-aarch64"  # static Linux binary for the in-box forwarder/hooks
//! shell = "/bin/sh"               # shell started in box panes
//! on_finish = "remove"            # remove | stop | keep (unsynced clones are stopped, never removed)
//! build = false                   # allow building devcontainer images (still needs repo trust)
//! cap_add = []                    # extra capabilities on top of --cap-drop ALL
//! caches = ["npm", "cargo"]       # shared named cache volumes (13 §5)
//! template = false                # commit a template image after setup; reuse it (13 §9)
//! warm_pool = 0                   # pre-started boxes per used template (13 §9)
//! warm_ttl = "2h"                 # warm boxes older than this are recycled
//! discover_ports = true           # forward listening box ports as previews (13 §4, 06)
//! ```
//!
//! Repo defaults live in `.vibeke/sandbox.toml` (13 §9, see [`RepoSandbox`]).

use crate::net::NetworkProfile;
use serde::{Deserialize, Serialize};
use vk_proto::model::IsolationLevel;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IsolationConfig {
    pub yolo_default: String,
    pub network: String,
    /// Vibeke-launched `--yolo --isolate host` needs a one-time confirmation per workspace.
    pub confirm_host_yolo: bool,
    /// Offer to relaunch a user-typed host yolo run (with `resume`) inside a sandbox.
    pub suggest_sandbox_for_yolo: bool,
    /// Pause idle container boxes after this long (`off` / `0` disables).
    pub idle_suspend: String,
    /// Share of a box limit (memory, pids, cpus) above which `sandbox.resource_pressure` fires.
    pub resource_pressure: f64,
    /// How often box resource usage is sampled.
    pub resource_poll: String,
    pub sandbox: SandboxConfig,
    pub container: ContainerConfig,
}

impl Default for IsolationConfig {
    fn default() -> Self {
        IsolationConfig {
            // 13 §3 says "vm if available, else container, else sandbox"; vm is M4 and a container
            // needs an image, so the default stays sandbox (set "container" explicitly).
            yolo_default: "sandbox".into(),
            network: "dev".into(),
            confirm_host_yolo: true,
            suggest_sandbox_for_yolo: false,
            idle_suspend: "30m".into(),
            resource_pressure: 0.9,
            resource_poll: "30s".into(),
            sandbox: SandboxConfig::default(),
            container: ContainerConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub read: Vec<String>,
    pub write: Vec<String>,
    pub allow_domains: Vec<String>,
    pub deny_domains: Vec<String>,
    pub local_ports: Vec<u16>,
    /// Ports allowlisted domains may be reached on (empty = 80 and 443, 13 §7).
    pub ports: Vec<u16>,
    pub allow_private: bool,
    pub broker: bool,
    /// TLS SNI / `Host` cross-check in the egress proxy (domain fronting, 13 §7).
    pub sni_check: bool,
    /// Egress Interactions offer "allow always" (persisted for every contained task).
    pub global_approvals: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig {
            read: vec![],
            write: vec![],
            allow_domains: vec![],
            deny_domains: vec![],
            local_ports: vec![],
            ports: vec![],
            allow_private: false,
            broker: true,
            sni_check: true,
            global_approvals: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContainerConfig {
    pub image: Option<String>,
    pub cpus: Option<String>,
    pub memory: Option<String>,
    pub pids: Option<u32>,
    pub runtime: Option<String>,
    pub code: String,
    pub vibeke_linux: Option<String>,
    pub shell: String,
    pub on_finish: String,
    pub build: bool,
    pub cap_add: Vec<String>,
    pub caches: Vec<String>,
    /// Commit a template image once a box's setup succeeded, and start later boxes of the same
    /// template from it with setup skipped (13 §9). Bind-mounted dirs are not in the image.
    pub template: bool,
    /// Pre-started boxes kept per used template (13 §9 warm pool; 0 = off).
    pub warm_pool: u32,
    /// Warm boxes older than this are removed and replaced.
    pub warm_ttl: String,
    /// Discover listening ports inside boxes and forward them as previews.
    pub discover_ports: bool,
}

impl Default for ContainerConfig {
    fn default() -> Self {
        ContainerConfig {
            image: None,
            cpus: None,
            memory: None,
            pids: None,
            runtime: None,
            code: "clone".into(),
            vibeke_linux: None,
            shell: "/bin/sh".into(),
            on_finish: "remove".into(),
            build: false,
            cap_add: vec![],
            caches: vec![],
            template: false,
            warm_pool: 0,
            warm_ttl: "2h".into(),
            discover_ports: true,
        }
    }
}

/// Named cache volumes per ecosystem (13 §5): `(volume, in-box path, env var)`.
pub fn cache_volume(name: &str) -> Option<(&'static str, &'static str, &'static str)> {
    Some(match name {
        "npm" => ("vibeke-cache-npm", "/vibeke/cache/npm", "npm_config_cache"),
        "pnpm" => (
            "vibeke-cache-pnpm",
            "/vibeke/cache/pnpm",
            "npm_config_store_dir",
        ),
        "cargo" => ("vibeke-cache-cargo", "/vibeke/cache/cargo", "CARGO_HOME"),
        "pip" => ("vibeke-cache-pip", "/vibeke/cache/pip", "PIP_CACHE_DIR"),
        "go" => ("vibeke-cache-go", "/vibeke/cache/go", "GOMODCACHE"),
        _ => return None,
    })
}

/// `.vibeke/sandbox.toml` (13 §9). Repo config can pick the image/devcontainer and *narrow*
/// resources and network; it can never widen network access (09).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoSandbox {
    pub image: Option<String>,
    pub devcontainer: Option<String>,
    pub network: Option<String>,
    pub cpus: Option<String>,
    pub memory: Option<String>,
}

impl RepoSandbox {
    /// Read `<checkout>/.vibeke/sandbox.toml` (`[sandbox]` table). Missing → default.
    pub fn load(checkout: &std::path::Path) -> Result<RepoSandbox, String> {
        let f = checkout.join(".vibeke/sandbox.toml");
        let Ok(text) = std::fs::read_to_string(&f) else {
            return Ok(RepoSandbox::default());
        };
        let t: toml::Table = text.parse().map_err(|e| format!("{}: {e}", f.display()))?;
        match t.get("sandbox") {
            Some(v) => v
                .clone()
                .try_into::<RepoSandbox>()
                .map_err(|e| format!("{}: {e}", f.display())),
            None => Ok(RepoSandbox::default()),
        }
    }
    /// The repo's network profile applies only when it is *narrower* than `current`.
    pub fn narrow_network(&self, current: NetworkProfile) -> NetworkProfile {
        let rank = |p: NetworkProfile| match p {
            NetworkProfile::None => 0,
            NetworkProfile::HarnessApis | NetworkProfile::PackageRegistries => 1,
            NetworkProfile::Dev => 2,
            NetworkProfile::Open => 3,
        };
        match self.network.as_deref().and_then(NetworkProfile::parse) {
            Some(p) if rank(p) < rank(current) => p,
            _ => current,
        }
    }
}

impl IsolationConfig {
    /// Parse from the raw `[isolation]` table (missing/invalid → defaults, with the error).
    pub fn from_toml(v: Option<&toml::Value>) -> (IsolationConfig, Option<String>) {
        match v {
            None => (IsolationConfig::default(), None),
            Some(v) => match v.clone().try_into::<IsolationConfig>() {
                Ok(c) => (c, None),
                Err(e) => (IsolationConfig::default(), Some(e.to_string())),
            },
        }
    }
    pub fn yolo_level(&self) -> IsolationLevel {
        IsolationLevel::parse(&self.yolo_default).unwrap_or(IsolationLevel::Sandbox)
    }
    pub fn network_profile(&self) -> NetworkProfile {
        NetworkProfile::parse(&self.network).unwrap_or_default()
    }
    /// `idle_suspend` as a duration (`None` = off).
    pub fn idle_suspend_after(&self) -> Option<std::time::Duration> {
        parse_duration(&self.idle_suspend).filter(|d| !d.is_zero())
    }
    pub fn resource_poll_every(&self) -> std::time::Duration {
        parse_duration(&self.resource_poll)
            .filter(|d| !d.is_zero())
            .unwrap_or(std::time::Duration::from_secs(30))
    }
    pub fn warm_ttl(&self) -> std::time::Duration {
        parse_duration(&self.container.warm_ttl)
            .filter(|d| !d.is_zero())
            .unwrap_or(std::time::Duration::from_secs(7200))
    }
}

/// `"30m"`, `"90s"`, `"2h"`, `"1d"`, `"500ms"` or a bare number of seconds. `off`, `never`,
/// `false` and empty → `None`.
pub fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim().to_ascii_lowercase();
    if matches!(s.as_str(), "" | "off" | "never" | "false" | "none") {
        return None;
    }
    let (num, mult_ms): (&str, u64) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86_400_000)
    } else {
        (s.as_str(), 1000)
    };
    let n: f64 = num.trim().parse().ok()?;
    if !(n.is_finite() && n >= 0.0) {
        return None;
    }
    Some(std::time::Duration::from_millis(
        (n * mult_ms as f64) as u64,
    ))
}

/// Expand `~/` against `home`.
pub fn expand(home: &std::path::Path, p: &str) -> std::path::PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => std::path::PathBuf::from(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_defaults() {
        let (c, e) = IsolationConfig::from_toml(None);
        assert!(e.is_none());
        assert_eq!(c.yolo_level(), IsolationLevel::Sandbox);
        assert_eq!(c.network_profile(), NetworkProfile::Dev);
        let v: toml::Value = toml::from_str(
            "yolo_default = \"host\"\nnetwork = \"harness-apis\"\n[sandbox]\nlocal_ports = [5432]\nallow_domains = [\"example.com\"]\n",
        )
        .unwrap();
        let (c, e) = IsolationConfig::from_toml(Some(&v));
        assert!(e.is_none(), "{e:?}");
        assert_eq!(c.yolo_level(), IsolationLevel::Host);
        assert_eq!(c.network_profile(), NetworkProfile::HarnessApis);
        assert_eq!(c.sandbox.local_ports, [5432]);
        assert!(c.sandbox.broker);
        let bad: toml::Value = toml::from_str("network = 3\n").unwrap();
        assert!(IsolationConfig::from_toml(Some(&bad)).1.is_some());
        let v: toml::Value = toml::from_str(
            "[container]\nimage = \"alpine:3.20\"\ncode = \"worktree\"\npids = 64\n",
        )
        .unwrap();
        let (c, e) = IsolationConfig::from_toml(Some(&v));
        assert!(e.is_none(), "{e:?}");
        assert_eq!(c.container.code, "worktree");
        assert_eq!(c.container.pids, Some(64));
        assert_eq!(c.container.shell, "/bin/sh");
        assert_eq!(c.container.on_finish, "remove");
    }

    #[test]
    fn spec_13_extras_defaults_and_durations() {
        use std::time::Duration;
        let (c, _) = IsolationConfig::from_toml(None);
        assert!(c.confirm_host_yolo);
        assert!(!c.suggest_sandbox_for_yolo);
        assert_eq!(c.idle_suspend_after(), Some(Duration::from_secs(1800)));
        assert!(c.sandbox.sni_check && c.sandbox.global_approvals);
        assert!(!c.container.template);
        assert_eq!(c.container.warm_pool, 0);
        assert!(c.container.discover_ports);
        assert_eq!(c.warm_ttl(), Duration::from_secs(7200));
        let v: toml::Value =
            toml::from_str("idle_suspend = \"off\"\nresource_poll = \"5s\"\n").unwrap();
        let (c, e) = IsolationConfig::from_toml(Some(&v));
        assert!(e.is_none(), "{e:?}");
        assert_eq!(c.idle_suspend_after(), None);
        assert_eq!(c.resource_poll_every(), Duration::from_secs(5));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("never"), None);
        assert_eq!(parse_duration("-3m"), None);
    }

    #[test]
    fn repo_sandbox_can_only_narrow_network() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(RepoSandbox::load(t.path()).unwrap(), RepoSandbox::default());
        std::fs::create_dir_all(t.path().join(".vibeke")).unwrap();
        std::fs::write(
            t.path().join(".vibeke/sandbox.toml"),
            "[sandbox]\nimage = \"node:22\"\nnetwork = \"open\"\ncpus = \"2\"\n",
        )
        .unwrap();
        let r = RepoSandbox::load(t.path()).unwrap();
        assert_eq!(r.image.as_deref(), Some("node:22"));
        assert_eq!(r.narrow_network(NetworkProfile::Dev), NetworkProfile::Dev);
        let r2 = RepoSandbox {
            network: Some("none".into()),
            ..Default::default()
        };
        assert_eq!(r2.narrow_network(NetworkProfile::Dev), NetworkProfile::None);
        assert_eq!(cache_volume("npm").unwrap().0, "vibeke-cache-npm");
        assert!(cache_volume("nope").is_none());
    }
}
