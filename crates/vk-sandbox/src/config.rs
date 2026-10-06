//! `[isolation]` user config (13 §3). The section is owned by this crate; `vk-config` keeps it
//! verbatim in `Config::extra`.
//!
//! ```toml
//! [isolation]
//! yolo_default = "sandbox"        # level applied by --yolo without --isolate
//! network = "dev"                 # default network profile for contained tasks
//!
//! [isolation.sandbox]
//! read = ["~/.zshrc"]             # extra home paths readable inside (read-only)
//! write = []                      # extra read-write paths (e.g. shared caches)
//! allow_domains = ["example.com"] # added to every profile except none
//! deny_domains = []
//! local_ports = [5432]            # loopback services reachable through the proxy
//! allow_private = false
//! broker = true                   # per-pane broker socket (hooks inside the box)
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
    pub allow_private: bool,
    pub broker: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig {
            read: vec![],
            write: vec![],
            allow_domains: vec![],
            deny_domains: vec![],
            local_ports: vec![],
            allow_private: false,
            broker: true,
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
