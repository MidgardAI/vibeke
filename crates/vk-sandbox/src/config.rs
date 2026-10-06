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
//! image = "ghcr.io/acme/dev:node22"
//! ```

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
            // 13 §3 says "vm if available, else container, else sandbox"; vm/container are not
            // wired for agents yet, so sandbox is the only enforced level this build offers.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ContainerConfig {
    pub image: Option<String>,
    pub cpus: Option<String>,
    pub memory: Option<String>,
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
    }
}
