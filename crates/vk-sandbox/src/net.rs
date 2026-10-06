//! Network profiles and the egress decision rules (13 §7): domain allowlists per profile,
//! per-task additions from egress Interactions, and resolved-IP checks that stop a contained
//! process from reaching loopback services, link-local/metadata endpoints or private ranges
//! through the proxy.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Network profile of a contained task (13 §7). `None` means no egress at all, not even the
/// proxy; every other profile routes through the host-side egress proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkProfile {
    /// No network: the sandbox profile denies every outbound connection.
    None,
    /// Only the model-provider endpoints the harnesses need (spec name: `offline`).
    HarnessApis,
    /// Package registries only (no model APIs): setup/install steps.
    PackageRegistries,
    /// Harness APIs + package registries (13 §7 default).
    #[default]
    Dev,
    /// Everything except forbidden IP classes; every connection is logged.
    Open,
}

impl NetworkProfile {
    pub fn parse(s: &str) -> Option<NetworkProfile> {
        Some(match s {
            "none" => NetworkProfile::None,
            "harness-apis" | "harness_apis" | "offline" => NetworkProfile::HarnessApis,
            "package-registries" | "package_registries" | "registries" => {
                NetworkProfile::PackageRegistries
            }
            "dev" => NetworkProfile::Dev,
            "open" => NetworkProfile::Open,
            _ => return None,
        })
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            NetworkProfile::None => "none",
            NetworkProfile::HarnessApis => "harness-apis",
            NetworkProfile::PackageRegistries => "package-registries",
            NetworkProfile::Dev => "dev",
            NetworkProfile::Open => "open",
        }
    }
    /// Does this profile use the egress proxy (as opposed to no network at all)?
    pub fn uses_proxy(&self) -> bool {
        *self != NetworkProfile::None
    }
    fn base_domains(&self) -> Vec<&'static str> {
        match self {
            NetworkProfile::None => vec![],
            NetworkProfile::HarnessApis => HARNESS_APIS.to_vec(),
            NetworkProfile::PackageRegistries => PACKAGE_REGISTRIES.to_vec(),
            NetworkProfile::Dev | NetworkProfile::Open => {
                let mut v = HARNESS_APIS.to_vec();
                v.extend_from_slice(PACKAGE_REGISTRIES);
                v
            }
        }
    }
}

/// Model-provider endpoints for the built-in harnesses (manifest `[sandbox.network]`, 04).
pub const HARNESS_APIS: &[&str] = &[
    // Claude Code
    "api.anthropic.com",
    "statsig.anthropic.com",
    "console.anthropic.com",
    "platform.claude.com",
    "claude.ai",
    // Codex
    "api.openai.com",
    "chatgpt.com",
    "auth.openai.com",
    // pi / omp providers
    "generativelanguage.googleapis.com",
    "openrouter.ai",
];

/// Package registries and download hosts (13 §7 `dev`).
pub const PACKAGE_REGISTRIES: &[&str] = &[
    "registry.npmjs.org",
    "registry.yarnpkg.com",
    "repo.yarnpkg.com",
    "pypi.org",
    "files.pythonhosted.org",
    "crates.io",
    "index.crates.io",
    "static.crates.io",
    "static.rust-lang.org",
    "proxy.golang.org",
    "sum.golang.org",
    "rubygems.org",
    "index.rubygems.org",
    "repo.maven.apache.org",
    "repo1.maven.org",
    "github.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "raw.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// `example.com` (exact), `*.example.com` (subdomains only) or `.example.com` (both).
pub fn domain_matches(pattern: &str, host: &str) -> bool {
    let p = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if p == "*" {
        return true;
    }
    if let Some(suffix) = p.strip_prefix("*.") {
        return h.len() > suffix.len() + 1
            && h.ends_with(suffix)
            && h.as_bytes()[h.len() - suffix.len() - 1] == b'.';
    }
    if let Some(suffix) = p.strip_prefix('.') {
        return h == suffix
            || (h.ends_with(suffix)
                && h.len() > suffix.len()
                && h.as_bytes()[h.len() - suffix.len() - 1] == b'.');
    }
    p == h
}

/// Why a resolved address is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpClass {
    Public,
    Loopback,
    Unspecified,
    LinkLocal,
    /// Cloud metadata endpoints (169.254.169.254, fd00:ec2::254).
    Metadata,
    Private,
    SharedCgnat,
    Multicast,
    Broadcast,
    Reserved,
}

impl IpClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            IpClass::Public => "public",
            IpClass::Loopback => "loopback",
            IpClass::Unspecified => "unspecified",
            IpClass::LinkLocal => "link_local",
            IpClass::Metadata => "metadata",
            IpClass::Private => "private",
            IpClass::SharedCgnat => "cgnat",
            IpClass::Multicast => "multicast",
            IpClass::Broadcast => "broadcast",
            IpClass::Reserved => "reserved",
        }
    }
}

fn classify_v4(ip: Ipv4Addr) -> IpClass {
    let o = ip.octets();
    if ip == Ipv4Addr::new(169, 254, 169, 254) || ip == Ipv4Addr::new(169, 254, 170, 2) {
        return IpClass::Metadata;
    }
    if ip.is_loopback() {
        IpClass::Loopback
    } else if ip.is_unspecified() || o[0] == 0 {
        IpClass::Unspecified
    } else if ip.is_link_local() {
        IpClass::LinkLocal
    } else if ip.is_private() {
        IpClass::Private
    } else if o[0] == 100 && (o[1] & 0xc0) == 64 {
        IpClass::SharedCgnat
    } else if ip.is_multicast() {
        IpClass::Multicast
    } else if ip.is_broadcast() {
        IpClass::Broadcast
    } else if o[0] >= 240
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] & 0xfe) == 18)
    {
        // 240/4 reserved, 192.0.0/24 IETF, 198.18/15 benchmarking.
        IpClass::Reserved
    } else {
        IpClass::Public
    }
}

/// Classify an address, looking through IPv4-mapped/compatible and NAT64 forms so
/// `::ffff:127.0.0.1` is loopback.
pub fn classify(ip: IpAddr) -> IpClass {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return classify_v4(v4);
            }
            let seg = v6.segments();
            // NAT64 well-known prefix 64:ff9b::/96.
            if seg[0] == 0x64 && seg[1] == 0xff9b && seg[2..6] == [0, 0, 0, 0] {
                let o = v6.octets();
                return classify_v4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
            }
            if v6 == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254) {
                return IpClass::Metadata;
            }
            if v6.is_loopback() {
                IpClass::Loopback
            } else if v6.is_unspecified() {
                IpClass::Unspecified
            } else if (seg[0] & 0xffc0) == 0xfe80 {
                IpClass::LinkLocal
            } else if (seg[0] & 0xfe00) == 0xfc00 {
                IpClass::Private
            } else if v6.is_multicast() {
                IpClass::Multicast
            } else if seg[0] == 0 && seg[1] == 0 && seg[2..6] == [0, 0, 0, 0] {
                // Deprecated IPv4-compatible ::a.b.c.d.
                let o = v6.octets();
                classify_v4(Ipv4Addr::new(o[12], o[13], o[14], o[15]))
            } else {
                IpClass::Public
            }
        }
    }
}

/// Outcome of the policy check for one destination, before DNS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostVerdict {
    Allow {
        rule: String,
    },
    /// Not on any list: ask the user (egress Interaction) or deny when nobody can answer.
    Ask,
    Deny {
        reason: String,
    },
}

/// Egress policy for one contained task (13 §7). Cloned into the proxy behind a lock; the
/// server adds `task_allow` entries when the user answers "allow for task".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EgressPolicy {
    pub profile: NetworkProfile,
    /// Extra domains from config, harness manifests and repo `.vibeke/sandbox.toml`.
    pub extra_allow: BTreeSet<String>,
    /// Domains approved for this task through egress Interactions.
    pub task_allow: BTreeSet<String>,
    /// Domains that are never allowed, even under `open` (checked first).
    pub deny: BTreeSet<String>,
    /// Loopback ports the user declared as reachable ("localhost services", 13 §7).
    pub local_ports: BTreeSet<u16>,
    /// Allow RFC 1918 / ULA destinations (off by default).
    pub allow_private: bool,
}

impl EgressPolicy {
    pub fn new(profile: NetworkProfile) -> Self {
        EgressPolicy {
            profile,
            ..Default::default()
        }
    }

    /// Policy decision for `host:port` before any DNS resolution. `host` is a domain or an IP
    /// literal (brackets already stripped).
    pub fn check_host(&self, host: &str, port: u16) -> HostVerdict {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() || port == 0 {
            return HostVerdict::Deny {
                reason: "invalid destination".into(),
            };
        }
        if self.profile == NetworkProfile::None {
            return HostVerdict::Deny {
                reason: "network profile none".into(),
            };
        }
        if let Some(d) = self.deny.iter().find(|d| domain_matches(d, &host)) {
            return HostVerdict::Deny {
                reason: format!("denied by rule {d}"),
            };
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            if classify(ip) == IpClass::Loopback {
                return if self.local_ports.contains(&port) {
                    HostVerdict::Allow {
                        rule: format!("localhost:{port}"),
                    }
                } else {
                    HostVerdict::Deny {
                        reason: format!("loopback port {port} not declared"),
                    }
                };
            }
            return if self.profile == NetworkProfile::Open {
                HostVerdict::Allow {
                    rule: "profile open".into(),
                }
            } else {
                HostVerdict::Ask
            };
        }
        if host == "localhost" || host.ends_with(".localhost") {
            return if self.local_ports.contains(&port) {
                HostVerdict::Allow {
                    rule: format!("localhost:{port}"),
                }
            } else {
                HostVerdict::Deny {
                    reason: format!("loopback port {port} not declared"),
                }
            };
        }
        if let Some(d) = self.task_allow.iter().find(|d| domain_matches(d, &host)) {
            return HostVerdict::Allow {
                rule: format!("task:{d}"),
            };
        }
        if let Some(d) = self.extra_allow.iter().find(|d| domain_matches(d, &host)) {
            return HostVerdict::Allow {
                rule: format!("extra:{d}"),
            };
        }
        if let Some(d) = self
            .profile
            .base_domains()
            .into_iter()
            .find(|d| domain_matches(d, &host))
        {
            return HostVerdict::Allow {
                rule: format!("{}:{d}", self.profile.as_str()),
            };
        }
        if self.profile == NetworkProfile::Open {
            return HostVerdict::Allow {
                rule: "profile open".into(),
            };
        }
        HostVerdict::Ask
    }

    /// May the proxy connect to `ip` for a request that passed [`check_host`]? Loopback only
    /// for declared local ports; link-local, metadata, private (unless allowed) and other
    /// special ranges never.
    pub fn check_ip(&self, ip: IpAddr, port: u16) -> Result<(), IpClass> {
        match classify(ip) {
            IpClass::Public => Ok(()),
            IpClass::Loopback if self.local_ports.contains(&port) => Ok(()),
            IpClass::Private if self.allow_private => Ok(()),
            c => Err(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_patterns() {
        assert!(domain_matches("example.com", "example.com"));
        assert!(domain_matches("example.com", "EXAMPLE.com."));
        assert!(!domain_matches("example.com", "www.example.com"));
        assert!(domain_matches("*.example.com", "www.example.com"));
        assert!(!domain_matches("*.example.com", "example.com"));
        assert!(!domain_matches("*.example.com", "badexample.com"));
        assert!(domain_matches(".example.com", "example.com"));
        assert!(domain_matches(".example.com", "a.b.example.com"));
        assert!(!domain_matches(".example.com", "evilexample.com"));
        assert!(domain_matches("*", "anything.test"));
    }

    #[test]
    fn ip_classes() {
        let c = |s: &str| classify(s.parse().unwrap());
        assert_eq!(c("127.0.0.1"), IpClass::Loopback);
        assert_eq!(c("127.8.9.10"), IpClass::Loopback);
        assert_eq!(c("::1"), IpClass::Loopback);
        assert_eq!(c("::ffff:127.0.0.1"), IpClass::Loopback);
        assert_eq!(c("64:ff9b::7f00:1"), IpClass::Loopback);
        assert_eq!(c("169.254.169.254"), IpClass::Metadata);
        assert_eq!(c("fd00:ec2::254"), IpClass::Metadata);
        assert_eq!(c("169.254.1.1"), IpClass::LinkLocal);
        assert_eq!(c("fe80::1"), IpClass::LinkLocal);
        assert_eq!(c("10.1.2.3"), IpClass::Private);
        assert_eq!(c("172.16.0.1"), IpClass::Private);
        assert_eq!(c("192.168.1.1"), IpClass::Private);
        assert_eq!(c("::ffff:192.168.1.1"), IpClass::Private);
        assert_eq!(c("fd12::1"), IpClass::Private);
        assert_eq!(c("100.64.0.1"), IpClass::SharedCgnat);
        assert_eq!(c("0.0.0.0"), IpClass::Unspecified);
        assert_eq!(c("::"), IpClass::Unspecified);
        assert_eq!(c("224.0.0.1"), IpClass::Multicast);
        assert_eq!(c("255.255.255.255"), IpClass::Broadcast);
        assert_eq!(c("240.0.0.1"), IpClass::Reserved);
        assert_eq!(c("93.184.215.14"), IpClass::Public);
        assert_eq!(c("2606:4700::1111"), IpClass::Public);
    }

    #[test]
    fn profiles_and_host_checks() {
        let p = EgressPolicy::new(NetworkProfile::Dev);
        assert!(matches!(
            p.check_host("api.anthropic.com", 443),
            HostVerdict::Allow { .. }
        ));
        assert!(matches!(
            p.check_host("registry.npmjs.org", 443),
            HostVerdict::Allow { .. }
        ));
        assert_eq!(p.check_host("example.com", 443), HostVerdict::Ask);
        // Loopback only for declared ports, by literal or by name.
        assert!(matches!(
            p.check_host("127.0.0.1", 5432),
            HostVerdict::Deny { .. }
        ));
        assert!(matches!(
            p.check_host("localhost", 5432),
            HostVerdict::Deny { .. }
        ));
        let mut p2 = p.clone();
        p2.local_ports.insert(5432);
        assert!(matches!(
            p2.check_host("127.0.0.1", 5432),
            HostVerdict::Allow { .. }
        ));
        // IP literals elsewhere are asked about (never silently allowed) except under open.
        assert_eq!(p.check_host("93.184.215.14", 443), HostVerdict::Ask);

        let h = EgressPolicy::new(NetworkProfile::HarnessApis);
        assert_eq!(h.check_host("registry.npmjs.org", 443), HostVerdict::Ask);
        let r = EgressPolicy::new(NetworkProfile::PackageRegistries);
        assert_eq!(r.check_host("api.anthropic.com", 443), HostVerdict::Ask);
        let n = EgressPolicy::new(NetworkProfile::None);
        assert!(matches!(
            n.check_host("api.anthropic.com", 443),
            HostVerdict::Deny { .. }
        ));
        let mut o = EgressPolicy::new(NetworkProfile::Open);
        assert!(matches!(
            o.check_host("example.com", 443),
            HostVerdict::Allow { .. }
        ));
        o.deny.insert("*.evil.test".into());
        assert!(matches!(
            o.check_host("x.evil.test", 443),
            HostVerdict::Deny { .. }
        ));
        // Task approvals.
        let mut t = EgressPolicy::new(NetworkProfile::HarnessApis);
        t.task_allow.insert("npmjs.org".into());
        assert!(matches!(
            t.check_host("npmjs.org", 443),
            HostVerdict::Allow { .. }
        ));
    }

    #[test]
    fn resolved_ip_checks() {
        let mut p = EgressPolicy::new(NetworkProfile::Open);
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(p.check_ip(ip("93.184.215.14"), 443).is_ok());
        assert_eq!(
            p.check_ip(ip("169.254.169.254"), 80),
            Err(IpClass::Metadata)
        );
        assert_eq!(p.check_ip(ip("127.0.0.1"), 80), Err(IpClass::Loopback));
        assert_eq!(p.check_ip(ip("10.0.0.1"), 80), Err(IpClass::Private));
        p.allow_private = true;
        assert!(p.check_ip(ip("10.0.0.1"), 80).is_ok());
        p.local_ports.insert(8080);
        assert!(p.check_ip(ip("127.0.0.1"), 8080).is_ok());
        assert_eq!(p.check_ip(ip("127.0.0.1"), 8081), Err(IpClass::Loopback));
    }

    #[test]
    fn profile_names() {
        for n in ["none", "harness-apis", "package-registries", "dev", "open"] {
            assert_eq!(NetworkProfile::parse(n).unwrap().as_str(), n);
        }
        assert_eq!(
            NetworkProfile::parse("offline"),
            Some(NetworkProfile::HarnessApis)
        );
        assert_eq!(NetworkProfile::parse("bogus"), None);
    }
}
