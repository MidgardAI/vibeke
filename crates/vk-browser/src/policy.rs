//! Destination policy for the agents' headless browser (spec 06 B5, 09 §8).
//!
//! Every request the headless browser makes is checked twice with the same [`Policy`]: by the
//! filtering proxy ([`crate::proxy`]), which resolves the name itself, checks **every resolved
//! address** and then connects only to those addresses (resolve once, pin — no second lookup a
//! rebinding DNS server could answer differently), and by the CDP `Fetch` layer in the server,
//! which also sees the resource type (top-level navigation vs subresource) and non-network
//! schemes (`file:`, `chrome:`) that never reach a proxy.
//!
//! Classes and defaults:
//!
//! | Destination | Default |
//! |---|---|
//! | loopback (`127/8`, `::1`, `0.0.0.0`, `::`) on a declared preview port | allow |
//! | other loopback ports | deny |
//! | cloud metadata (`169.254.169.254`, `fd00:ec2::254`, `100.100.100.200`) | deny unless an allow rule names the address |
//! | link-local (`169.254/16`, `fe80::/10`) | deny unless allow-listed |
//! | private (RFC 1918, CGNAT `100.64/10`, ULA `fc00::/7`, `198.18/15`) | deny unless allow-listed |
//! | multicast, broadcast, reserved, documentation | deny |
//! | public | per [`External`]: subresources only by default |

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// What kind of address an IP is, after unwrapping IPv4-mapped/compatible and NAT64 forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpClass {
    Loopback,
    Metadata,
    LinkLocal,
    Private,
    Reserved,
    Public,
}

impl IpClass {
    pub fn as_str(self) -> &'static str {
        match self {
            IpClass::Loopback => "loopback",
            IpClass::Metadata => "metadata",
            IpClass::LinkLocal => "link_local",
            IpClass::Private => "private",
            IpClass::Reserved => "reserved",
            IpClass::Public => "public",
        }
    }
}

/// `::ffff:a.b.c.d`, `::a.b.c.d` (deprecated compatible form) and NAT64 `64:ff9b::a.b.c.d` are
/// classified as the IPv4 address they carry.
pub fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let s = v6.segments();
            if s[..6] == [0, 0, 0, 0, 0, 0] && !(s[6] == 0 && s[7] <= 1) {
                // ::a.b.c.d (not :: or ::1)
                return IpAddr::V4(Ipv4Addr::new(
                    (s[6] >> 8) as u8,
                    s[6] as u8,
                    (s[7] >> 8) as u8,
                    s[7] as u8,
                ));
            }
            if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return IpAddr::V4(Ipv4Addr::new(
                    (s[6] >> 8) as u8,
                    s[6] as u8,
                    (s[7] >> 8) as u8,
                    s[7] as u8,
                ));
            }
            IpAddr::V6(v6)
        }
        v4 => v4,
    }
}

fn in_v4(ip: Ipv4Addr, net: [u8; 4], bits: u32) -> bool {
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    u32::from(ip) & mask == u32::from(Ipv4Addr::from(net)) & mask
}

fn in_v6(ip: Ipv6Addr, net: Ipv6Addr, bits: u32) -> bool {
    let mask = if bits == 0 {
        0
    } else {
        u128::MAX << (128 - bits)
    };
    u128::from(ip) & mask == u128::from(net) & mask
}

pub fn classify(ip: IpAddr) -> IpClass {
    match canonical(ip) {
        IpAddr::V4(a) => {
            if a.is_loopback() || a.is_unspecified() {
                IpClass::Loopback
            } else if a == Ipv4Addr::new(169, 254, 169, 254)
                || a == Ipv4Addr::new(169, 254, 170, 2)
                || a == Ipv4Addr::new(100, 100, 100, 200)
            {
                IpClass::Metadata
            } else if in_v4(a, [169, 254, 0, 0], 16) {
                IpClass::LinkLocal
            } else if a.is_private()
                || in_v4(a, [100, 64, 0, 0], 10)
                || in_v4(a, [198, 18, 0, 0], 15)
            {
                IpClass::Private
            } else if in_v4(a, [0, 0, 0, 0], 8)
                || a.is_multicast()
                || a.is_broadcast()
                || in_v4(a, [240, 0, 0, 0], 4)
                || in_v4(a, [192, 0, 0, 0], 24)
                || in_v4(a, [192, 0, 2, 0], 24)
                || in_v4(a, [198, 51, 100, 0], 24)
                || in_v4(a, [203, 0, 113, 0], 24)
            {
                IpClass::Reserved
            } else {
                IpClass::Public
            }
        }
        IpAddr::V6(a) => {
            if a.is_loopback() || a.is_unspecified() {
                IpClass::Loopback
            } else if a == "fd00:ec2::254".parse::<Ipv6Addr>().expect("literal") {
                IpClass::Metadata
            } else if in_v6(a, "fe80::".parse().expect("literal"), 10) {
                IpClass::LinkLocal
            } else if in_v6(a, "fc00::".parse().expect("literal"), 7)
                || in_v6(a, "fec0::".parse().expect("literal"), 10)
            {
                IpClass::Private
            } else if a.is_multicast()
                || in_v6(a, "2001:db8::".parse().expect("literal"), 32)
                || in_v6(a, "100::".parse().expect("literal"), 64)
                || in_v6(a, "::".parse().expect("literal"), 8)
            {
                IpClass::Reserved
            } else {
                IpClass::Public
            }
        }
    }
}

/// `preview.browser_external`: what public destinations may be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum External {
    Deny,
    /// Subresources (CDNs, fonts) yes, top-level navigation no.
    #[default]
    Subresources,
    Allow,
}

impl External {
    pub fn parse(s: &str) -> Option<External> {
        match s {
            "deny" => Some(External::Deny),
            "subresources" => Some(External::Subresources),
            "allow" => Some(External::Allow),
            _ => None,
        }
    }
}

/// One `preview.browser_allow_private` entry: a CIDR (`10.0.0.0/8`, `192.168.1.5`) or a host
/// name (`db.internal`), each with an optional `:port` (`[::1]:5432` for IPv6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowRule {
    Cidr {
        net: IpAddr,
        bits: u8,
        port: Option<u16>,
    },
    Host {
        host: String,
        port: Option<u16>,
    },
}

impl AllowRule {
    pub fn parse(s: &str) -> Option<AllowRule> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        // Split an optional port: `[v6]:p`, `v4:p`, `host:p`, `cidr:p` (a bare v6 has >1 colon).
        let (body, port) = if let Some(rest) = s.strip_prefix('[') {
            let (inner, after) = rest.split_once(']')?;
            let port = match after.strip_prefix(':') {
                Some(p) => Some(p.parse().ok()?),
                None if after.is_empty() => None,
                None => return None,
            };
            (inner.to_string(), port)
        } else if s.matches(':').count() == 1 {
            let (h, p) = s.split_once(':')?;
            (h.to_string(), Some(p.parse().ok()?))
        } else {
            (s.to_string(), None)
        };
        let (addr, bits) = match body.split_once('/') {
            Some((a, b)) => (a.to_string(), Some(b.parse::<u8>().ok()?)),
            None => (body.clone(), None),
        };
        if let Ok(ip) = addr.parse::<IpAddr>() {
            let max = if ip.is_ipv4() { 32 } else { 128 };
            let bits = bits.unwrap_or(max);
            if bits > max {
                return None;
            }
            return Some(AllowRule::Cidr {
                net: ip,
                bits,
                port,
            });
        }
        if bits.is_some()
            || !body
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
        {
            return None;
        }
        Some(AllowRule::Host {
            host: body.to_ascii_lowercase(),
            port,
        })
    }

    fn port_ok(p: Option<u16>, port: u16) -> bool {
        p.is_none_or(|p| p == port)
    }

    fn matches_ip(&self, ip: IpAddr, port: u16) -> bool {
        match self {
            AllowRule::Cidr { net, bits, port: p } => {
                let ok = match (canonical(*net), canonical(ip)) {
                    (IpAddr::V4(n), IpAddr::V4(a)) => {
                        // A v4-mapped v6 rule (`::ffff:10.0.0.0/104`) counts its prefix from
                        // bit 96; one shorter than that cannot describe a v4 range (fuzz: this
                        // used to overflow the shift).
                        let bits = if net.is_ipv6() {
                            u32::from(*bits).checked_sub(96)
                        } else {
                            Some(u32::from(*bits))
                        };
                        bits.is_some_and(|b| in_v4(a, n.octets(), b))
                    }
                    (IpAddr::V6(n), IpAddr::V6(a)) => in_v6(a, n, u32::from(*bits)),
                    _ => false,
                };
                ok && Self::port_ok(*p, port)
            }
            AllowRule::Host { .. } => false,
        }
    }

    fn matches_host(&self, host: &str, port: u16) -> bool {
        match self {
            AllowRule::Host { host: h, port: p } => {
                h.eq_ignore_ascii_case(host.trim_end_matches('.')) && Self::port_ok(*p, port)
            }
            AllowRule::Cidr { .. } => false,
        }
    }
}

/// How the request is used. The proxy can't tell (`Unknown`); the Fetch layer can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Navigation,
    Subresource,
    Unknown,
}

/// Why a destination was allowed or denied (stable strings in logs, events and errors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    PreviewPort,
    AllowListed,
    External,
    LoopbackPort,
    Metadata,
    LinkLocal,
    Private,
    Reserved,
    ExternalDenied,
    ExternalNavigation,
    Scheme,
    Unresolvable,
    BadTarget,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::PreviewPort => "preview_port",
            Reason::AllowListed => "allow_listed",
            Reason::External => "external",
            Reason::LoopbackPort => "loopback_port_not_a_preview",
            Reason::Metadata => "metadata_address",
            Reason::LinkLocal => "link_local_address",
            Reason::Private => "private_address",
            Reason::Reserved => "reserved_address",
            Reason::ExternalDenied => "external_denied",
            Reason::ExternalNavigation => "external_navigation",
            Reason::Scheme => "scheme_not_allowed",
            Reason::Unresolvable => "unresolvable",
            Reason::BadTarget => "bad_target",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub allow: bool,
    pub reason: Reason,
    /// The class of the deciding address (the first denied one, or the last allowed one).
    pub class: Option<IpClass>,
}

impl Decision {
    fn allow(reason: Reason, class: IpClass) -> Decision {
        Decision {
            allow: true,
            reason,
            class: Some(class),
        }
    }
    fn deny(reason: Reason, class: Option<IpClass>) -> Decision {
        Decision {
            allow: false,
            reason,
            class,
        }
    }
    pub fn denied(reason: Reason) -> Decision {
        Decision::deny(reason, None)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    /// Loopback ports that belong to this machine's declared previews.
    pub preview_ports: BTreeSet<u16>,
    pub allow: Vec<AllowRule>,
    pub external: External,
}

impl Policy {
    /// Decide for one resolved address. `host` is the name as requested (for host rules).
    pub fn decide_ip(&self, host: &str, ip: IpAddr, port: u16, kind: Kind) -> Decision {
        let class = classify(ip);
        let host_rule = self.allow.iter().any(|r| r.matches_host(host, port));
        let ip_rule = self.allow.iter().any(|r| r.matches_ip(ip, port));
        match class {
            IpClass::Loopback if self.preview_ports.contains(&port) => {
                Decision::allow(Reason::PreviewPort, class)
            }
            // Metadata endpoints need a rule naming the address itself, never a host rule.
            IpClass::Metadata if ip_rule => Decision::allow(Reason::AllowListed, class),
            IpClass::Metadata => Decision::deny(Reason::Metadata, Some(class)),
            IpClass::Reserved => Decision::deny(Reason::Reserved, Some(class)),
            IpClass::Loopback | IpClass::LinkLocal | IpClass::Private if ip_rule || host_rule => {
                Decision::allow(Reason::AllowListed, class)
            }
            IpClass::Loopback => Decision::deny(Reason::LoopbackPort, Some(class)),
            IpClass::LinkLocal => Decision::deny(Reason::LinkLocal, Some(class)),
            IpClass::Private => Decision::deny(Reason::Private, Some(class)),
            IpClass::Public => match (self.external, kind) {
                _ if ip_rule || host_rule => Decision::allow(Reason::AllowListed, class),
                (External::Deny, _) => Decision::deny(Reason::ExternalDenied, Some(class)),
                (External::Subresources, Kind::Navigation) => {
                    Decision::deny(Reason::ExternalNavigation, Some(class))
                }
                _ => Decision::allow(Reason::External, class),
            },
        }
    }

    /// Decide for a name and **all** of its resolved addresses: every address must be allowed
    /// (a name answering with one public and one internal address is denied), and the caller
    /// then connects only to these addresses.
    pub fn decide(&self, host: &str, port: u16, ips: &[IpAddr], kind: Kind) -> Decision {
        if ips.is_empty() {
            return Decision::denied(Reason::Unresolvable);
        }
        let mut last = Decision::denied(Reason::Unresolvable);
        for ip in ips {
            let d = self.decide_ip(host, *ip, port, kind);
            if !d.allow {
                return d;
            }
            last = d;
        }
        last
    }
}

/// The parts of a URL the policy needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub scheme: String,
    /// Lower-case, IPv6 brackets stripped.
    pub host: String,
    pub port: u16,
}

/// Parse `scheme://[user@]host[:port]/…` for http/https/ws/wss. Other schemes → `None`.
pub fn parse_target(url: &str) -> Option<Target> {
    if url.chars().any(|c| c.is_control() || c == ' ') {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" | "ws" => 80,
        "https" | "wss" => 443,
        _ => return None,
    };
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit('@').next()?;
    let (host, port) = split_host_port(auth, default_port)?;
    Some(Target { scheme, host, port })
}

/// `host[:port]` / `[v6][:port]` → (host without brackets, lower-cased; port).
pub fn split_host_port(auth: &str, default_port: u16) -> Option<(String, u16)> {
    let (host, port) = if let Some(r) = auth.strip_prefix('[') {
        let (h, after) = r.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => default_port,
            None => return None,
        };
        (h.to_string(), port)
    } else {
        match auth.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h.to_string(), p.parse().ok()?),
            Some(_) => return None,
            None => (auth.to_string(), default_port),
        }
    };
    if host.is_empty() || port == 0 {
        return None;
    }
    Some((host.to_ascii_lowercase(), port))
}

/// Schemes a page may load without a network request: allowed without a destination check.
pub fn is_local_scheme(url: &str) -> bool {
    let l = url.get(..12).unwrap_or(url).to_ascii_lowercase();
    l.starts_with("data:")
        || l.starts_with("blob:")
        || l == "about:blank"
        || l.starts_with("about:blank")
}

/// Names that always mean this machine's loopback (never sent to DNS).
pub fn is_localhost_name(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "localhost" || h.ends_with(".localhost")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn policy(ports: &[u16]) -> Policy {
        Policy {
            preview_ports: ports.iter().copied().collect(),
            ..Policy::default()
        }
    }

    #[test]
    fn ip_classes() {
        for (s, c) in [
            ("127.0.0.1", IpClass::Loopback),
            ("127.8.9.10", IpClass::Loopback),
            ("0.0.0.0", IpClass::Loopback),
            ("::1", IpClass::Loopback),
            ("::", IpClass::Loopback),
            ("::ffff:127.0.0.1", IpClass::Loopback),
            ("169.254.169.254", IpClass::Metadata),
            ("::ffff:169.254.169.254", IpClass::Metadata),
            ("64:ff9b::a9fe:a9fe", IpClass::Metadata),
            ("fd00:ec2::254", IpClass::Metadata),
            ("100.100.100.200", IpClass::Metadata),
            ("169.254.1.1", IpClass::LinkLocal),
            ("fe80::1", IpClass::LinkLocal),
            ("10.1.2.3", IpClass::Private),
            ("172.16.0.1", IpClass::Private),
            ("172.31.255.255", IpClass::Private),
            ("192.168.1.1", IpClass::Private),
            ("100.64.0.1", IpClass::Private),
            ("100.127.255.254", IpClass::Private),
            ("fd12:3456::1", IpClass::Private),
            ("198.18.0.1", IpClass::Private),
            ("224.0.0.1", IpClass::Reserved),
            ("255.255.255.255", IpClass::Reserved),
            ("240.0.0.1", IpClass::Reserved),
            ("0.1.2.3", IpClass::Reserved),
            ("192.0.2.1", IpClass::Reserved),
            ("ff02::1", IpClass::Reserved),
            ("2001:db8::1", IpClass::Reserved),
            ("93.184.216.34", IpClass::Public),
            ("172.32.0.1", IpClass::Public),
            ("100.128.0.1", IpClass::Public),
            ("2606:4700::1111", IpClass::Public),
        ] {
            assert_eq!(classify(ip(s)), c, "{s}");
        }
    }

    #[test]
    fn loopback_only_on_preview_ports() {
        let p = policy(&[5173]);
        let d = p.decide(
            "localhost",
            5173,
            &[ip("127.0.0.1"), ip("::1")],
            Kind::Navigation,
        );
        assert!(d.allow);
        assert_eq!(d.reason, Reason::PreviewPort);
        let d = p.decide("localhost", 5432, &[ip("127.0.0.1")], Kind::Subresource);
        assert!(!d.allow);
        assert_eq!(d.reason, Reason::LoopbackPort);
        // 0.0.0.0 reaches loopback: same rule.
        assert!(
            !p.decide("0.0.0.0", 22, &[ip("0.0.0.0")], Kind::Unknown)
                .allow
        );
        assert!(
            p.decide("0.0.0.0", 5173, &[ip("0.0.0.0")], Kind::Unknown)
                .allow
        );
    }

    #[test]
    fn metadata_link_local_private_denied_by_default() {
        let p = policy(&[80]);
        for (s, r) in [
            ("169.254.169.254", Reason::Metadata),
            ("169.254.3.4", Reason::LinkLocal),
            ("10.0.0.5", Reason::Private),
            ("100.101.102.103", Reason::Private),
            ("224.0.0.251", Reason::Reserved),
        ] {
            let d = p.decide("x", 80, &[ip(s)], Kind::Subresource);
            assert!(!d.allow, "{s}");
            assert_eq!(d.reason, r, "{s}");
        }
    }

    #[test]
    fn allow_rules() {
        let mut p = policy(&[]);
        p.allow = [
            "10.0.0.0/8",
            "db.internal:5432",
            "[::1]:9229",
            "192.168.1.5",
        ]
        .iter()
        .map(|s| AllowRule::parse(s).unwrap())
        .collect();
        assert!(
            p.decide("x", 443, &[ip("10.9.8.7")], Kind::Navigation)
                .allow
        );
        assert!(
            p.decide("db.internal", 5432, &[ip("172.20.0.3")], Kind::Unknown)
                .allow
        );
        assert!(
            !p.decide("db.internal", 5433, &[ip("172.20.0.3")], Kind::Unknown)
                .allow
        );
        assert!(p.decide("::1", 9229, &[ip("::1")], Kind::Unknown).allow);
        assert!(!p.decide("::1", 9230, &[ip("::1")], Kind::Unknown).allow);
        assert!(p.decide("y", 80, &[ip("192.168.1.5")], Kind::Unknown).allow);
        assert!(!p.decide("y", 80, &[ip("192.168.1.6")], Kind::Unknown).allow);
        // A host rule never unlocks a metadata address; a CIDR naming it does.
        p.allow.push(AllowRule::parse("meta.example").unwrap());
        assert!(
            !p.decide("meta.example", 80, &[ip("169.254.169.254")], Kind::Unknown)
                .allow
        );
        p.allow
            .push(AllowRule::parse("169.254.169.254/32").unwrap());
        assert!(
            p.decide("meta.example", 80, &[ip("169.254.169.254")], Kind::Unknown)
                .allow
        );
        // Parsing.
        assert_eq!(AllowRule::parse(""), None);
        assert_eq!(AllowRule::parse("10.0.0.0/33"), None);
        assert_eq!(AllowRule::parse("bad host"), None);
        assert_eq!(AllowRule::parse("x:notaport"), None);
        assert!(matches!(
            AllowRule::parse("fd00::/8"),
            Some(AllowRule::Cidr {
                bits: 8,
                port: None,
                ..
            })
        ));
    }

    /// Found by the vk-fuzz `policy_match` target: a v4-mapped v6 rule with a /128 prefix
    /// shifted a u32 by a negative amount.
    #[test]
    fn mapped_v6_rule_prefix_is_relative_to_bit_96() {
        let mut p = Policy::default();
        p.allow
            .push(AllowRule::parse("::ffff:10.0.0.7/128").unwrap());
        assert!(p.decide_ip("x", ip("10.0.0.7"), 80, Kind::Unknown).allow);
        assert!(!p.decide_ip("x", ip("10.0.0.8"), 80, Kind::Unknown).allow);
        let mut p = Policy::default();
        p.allow
            .push(AllowRule::parse("::ffff:10.0.0.0/104").unwrap());
        assert!(p.decide_ip("x", ip("10.9.9.9"), 80, Kind::Unknown).allow);
        // A prefix shorter than 96 cannot describe a v4 range: no match, no panic.
        let mut p = Policy::default();
        p.allow.push(AllowRule::parse("::ffff:10.0.0.0/8").unwrap());
        assert!(!p.decide_ip("x", ip("10.0.0.1"), 80, Kind::Unknown).allow);
    }

    #[test]
    fn external_modes() {
        let mut p = policy(&[]);
        let pubip = [ip("93.184.216.34")];
        assert!(
            p.decide("cdn.example", 443, &pubip, Kind::Subresource)
                .allow
        );
        assert!(p.decide("cdn.example", 443, &pubip, Kind::Unknown).allow);
        let d = p.decide("example.com", 443, &pubip, Kind::Navigation);
        assert_eq!(d.reason, Reason::ExternalNavigation);
        p.external = External::Deny;
        assert_eq!(
            p.decide("cdn.example", 443, &pubip, Kind::Subresource)
                .reason,
            Reason::ExternalDenied
        );
        p.external = External::Allow;
        assert!(p.decide("example.com", 443, &pubip, Kind::Navigation).allow);
    }

    #[test]
    fn every_resolved_address_must_pass() {
        let p = Policy {
            external: External::Allow,
            ..policy(&[3000])
        };
        // A rebinding-style answer mixing a public and an internal address is refused.
        let d = p.decide(
            "evil.example",
            80,
            &[ip("93.184.216.34"), ip("127.0.0.1")],
            Kind::Unknown,
        );
        assert!(!d.allow);
        assert_eq!(d.reason, Reason::LoopbackPort);
        assert_eq!(
            p.decide("x", 80, &[], Kind::Unknown).reason,
            Reason::Unresolvable
        );
    }

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("http://localhost:5173/app?x#y"),
            Some(Target {
                scheme: "http".into(),
                host: "localhost".into(),
                port: 5173
            })
        );
        assert_eq!(parse_target("HTTPS://Example.COM/").unwrap().port, 443);
        assert_eq!(parse_target("ws://[::1]:9000/hmr").unwrap().host, "::1");
        assert_eq!(
            parse_target("wss://u:p@h.example").unwrap().host,
            "h.example"
        );
        assert_eq!(parse_target("file:///etc/passwd"), None);
        assert_eq!(parse_target("chrome://settings"), None);
        assert_eq!(parse_target("http://h:0/"), None);
        assert_eq!(parse_target("http://h:99999/"), None);
        assert_eq!(parse_target("http://a b/"), None);
        assert!(is_local_scheme("data:text/html,hi"));
        assert!(is_local_scheme("about:blank"));
        assert!(!is_local_scheme("about:config"));
        assert!(!is_local_scheme("file:///"));
        assert!(is_localhost_name("LOCALHOST."));
        assert!(is_localhost_name("app.localhost"));
        assert!(!is_localhost_name("localhost.evil.com"));
    }
}
