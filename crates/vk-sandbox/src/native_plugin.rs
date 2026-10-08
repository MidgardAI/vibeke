//! OS sandbox for native (scoped) plugins (09 §6 "M6: OS-level sandbox for scoped process
//! plugins … macOS `sandbox-exec` profile generated from declared capabilities; plugins can
//! opt in early via `sandbox = true`"; 07 §7.3 resource limits).
//!
//! The profile is generated from the plugin's approved capabilities:
//!
//! * the plugin directory and its config directory: read-only;
//! * its state (data) directory and a private tmp/cache below it: read-write;
//! * `filesystem = [...]` entries: `$PLUGIN_DATA` / `$PLUGIN_CONFIG` / `$HOME/…` / absolute
//!   paths, read-write unless suffixed `:ro`;
//! * the system, the home allowlist of [`crate::policy::DEFAULT_HOME_READ`]; nothing else of the
//!   home and none of Vibeke's runtime, state or config (other plugins, the registry);
//! * Unix sockets: only the session socket the plugin was given a token for (argv actions);
//! * network: none when `network` is empty, unrestricted for `network = ["*"]`, and otherwise
//!   only the per-plugin egress proxy ([`egress_policy`], [`crate::proxy`]) that allows exactly
//!   the declared hosts (ports 80/443) and denies everything else without asking.
//!
//! Linux runs the same policy through bubblewrap + Landlock + seccomp (`vibeke sandbox bwrap`);
//! that chain is generated and unit-tested here but has not been verified on a Linux host.
//! [`ResourceLimits`] are applied with `setrlimit` in the child before exec, sandboxed or not.

use crate::env;
use crate::net::{EgressPolicy, HARNESS_APIS, NetworkProfile, domain_matches};
use crate::policy::{NetMode, Policy, SandboxSpec};
use std::path::{Path, PathBuf};

pub use crate::plugin::{Prepared, probe};

/// How the plugin reaches the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Net {
    None,
    Open,
    /// Through the host-side egress proxy on `127.0.0.1:<port>`.
    Proxy(u16),
}

/// Network mode for a declared host list: empty → none, `*` anywhere → open, else proxy.
pub fn net_for(hosts: &[String], proxy_port: Option<u16>) -> Net {
    if hosts.is_empty() {
        Net::None
    } else if hosts.iter().any(|h| h.trim() == "*") {
        Net::Open
    } else {
        match proxy_port {
            Some(p) => Net::Proxy(p),
            None => Net::None,
        }
    }
}

/// The egress policy for a plugin's proxy: the declared hosts on ports 80/443, nothing else,
/// never an ask (the proxy runs without an asker, so anything unlisted is denied).
pub fn egress_policy(hosts: &[String]) -> EgressPolicy {
    // A profile that uses the proxy but whose base list does not apply: the harness API
    // domains it would add are denied unless the plugin declared them itself.
    let mut p = EgressPolicy::new(NetworkProfile::HarnessApis);
    for h in hosts {
        p.extra_allow.insert(h.trim().to_ascii_lowercase());
    }
    for d in HARNESS_APIS {
        if !hosts
            .iter()
            .any(|h| domain_matches(d, &h.trim().to_ascii_lowercase()))
        {
            p.deny.insert((*d).to_string());
        }
    }
    p
}

/// `setrlimit` values for a plugin process (07 §7.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceLimits {
    pub memory_mb: Option<u64>,
    pub cpu_seconds: Option<u64>,
    pub open_files: Option<u64>,
}

impl ResourceLimits {
    /// Apply in the child between fork and exec (async-signal-safe: only `setrlimit`).
    /// Memory is `RLIMIT_AS` on Linux and `RLIMIT_DATA` elsewhere (macOS does not enforce
    /// `RLIMIT_AS`).
    pub fn apply(&self) -> std::io::Result<()> {
        fn set(res: libc::c_int, v: u64) -> std::io::Result<()> {
            let lim = libc::rlimit {
                rlim_cur: v as libc::rlim_t,
                rlim_max: v as libc::rlim_t,
            };
            // SAFETY: plain syscall on a stack value.
            if unsafe { libc::setrlimit(res as _, &lim) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
        if let Some(mb) = self.memory_mb {
            #[cfg(target_os = "linux")]
            let res = libc::RLIMIT_AS as libc::c_int;
            #[cfg(not(target_os = "linux"))]
            let res = libc::RLIMIT_DATA as libc::c_int;
            set(res, mb.saturating_mul(1024 * 1024))?;
        }
        if let Some(s) = self.cpu_seconds {
            set(libc::RLIMIT_CPU as libc::c_int, s)?;
        }
        if let Some(n) = self.open_files {
            set(libc::RLIMIT_NOFILE as libc::c_int, n)?;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.memory_mb.is_none() && self.cpu_seconds.is_none() && self.open_files.is_none()
    }
}

/// One `filesystem` entry resolved: (path, writable).
pub fn resolve_filesystem(
    entries: &[String],
    home: &Path,
    data: &Path,
    config: &Path,
) -> Vec<(PathBuf, bool)> {
    entries
        .iter()
        .filter_map(|e| {
            let ro = e.ends_with(":ro");
            let base = e.trim_end_matches(":ro");
            if base.split('/').any(|c| c == "..") {
                return None;
            }
            let expand = |prefix: &str, root: &Path| {
                base.strip_prefix(prefix).map(|rest| {
                    let rest = rest.trim_start_matches('/');
                    if rest.is_empty() {
                        root.to_path_buf()
                    } else {
                        root.join(rest)
                    }
                })
            };
            let p = expand("$PLUGIN_DATA", data)
                .or_else(|| expand("$PLUGIN_CONFIG", config))
                .or_else(|| expand("$HOME", home))
                .or_else(|| base.starts_with('/').then(|| PathBuf::from(base)))?;
            Some((p, !ro))
        })
        .collect()
}

/// Everything one sandboxed native plugin command needs.
#[derive(Debug, Clone)]
pub struct NativeBox {
    pub home: PathBuf,
    pub plugin_root: PathBuf,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    /// Vibeke's directories to hide (state root, runtime root, config dir).
    pub hidden: Vec<PathBuf>,
    /// Read-only extras (the `vibeke` binary).
    pub extra_read: Vec<PathBuf>,
    /// Resolved `filesystem` grants.
    pub filesystem: Vec<(PathBuf, bool)>,
    /// Unix sockets it may connect to (the session socket for argv actions).
    pub sockets: Vec<PathBuf>,
    pub net: Net,
    pub vibeke_bin: Option<PathBuf>,
    /// Where the generated profile is written (outside every plugin-writable path).
    pub profile_dir: PathBuf,
}

impl NativeBox {
    pub fn spec(&self) -> SandboxSpec {
        let mut extra_read = vec![self.plugin_root.clone(), self.config_dir.clone()];
        extra_read.extend(self.extra_read.iter().cloned());
        let mut extra_write = vec![];
        for (p, w) in &self.filesystem {
            if *w {
                extra_write.push(p.clone());
            } else {
                extra_read.push(p.clone());
            }
        }
        SandboxSpec {
            home: self.home.clone(),
            checkout: self.state_dir.clone(),
            git: None,
            private_dir: self.state_dir.join(".sandbox"),
            extra_read,
            extra_write,
            read_only_files: vec![],
            hidden: self.hidden.clone(),
            home_read: None,
            unix_sockets: self.sockets.clone(),
            network: match self.net {
                Net::None => NetMode::None,
                Net::Open => NetMode::Open,
                Net::Proxy(port) => NetMode::Proxy {
                    port,
                    local_ports: vec![],
                },
            },
            allow_bind_localhost: false,
            protected: vec![],
            control_dir: Some(self.profile_dir.clone()),
        }
    }

    /// Wrap `argv` for execution in `cwd` with the scrubbed environment plus `plugin_env`
    /// (the plugin's own `VIBEKE_*` values).
    pub fn prepare(
        &self,
        argv: &[String],
        cwd: &Path,
        plugin_env: Vec<(String, String)>,
        id: &str,
    ) -> Result<Prepared, String> {
        probe()?;
        std::fs::create_dir_all(&self.state_dir).map_err(|e| e.to_string())?;
        let private = crate::fsafe::ensure_dir_under(&self.state_dir, Path::new(".sandbox"))
            .map_err(|e| format!("sandbox dir: {e}"))?;
        let policy = Policy::from_spec(&self.spec());
        let _ = std::fs::create_dir_all(&self.profile_dir);
        let profile = self.profile_dir.join(format!(
            "{id}.{}",
            if cfg!(target_os = "linux") {
                "policy.json"
            } else {
                "sb"
            }
        ));
        let bytes = if cfg!(target_os = "linux") {
            serde_json::to_vec_pretty(&policy).map_err(|e| e.to_string())?
        } else {
            crate::seatbelt::render(&policy).into_bytes()
        };
        let _ = std::fs::remove_file(&profile);
        crate::fsafe::write_nofollow(&profile, &bytes, 0o600)
            .map_err(|e| format!("profile: {e}"))?;
        let host: Vec<(String, String)> = std::env::vars().collect();
        let mut e = env::scrub(&host);
        for (k, v) in plugin_env {
            env::set(&mut e, &k, v);
        }
        env::private_dirs(&mut e, &private).map_err(|e| format!("sandbox dirs: {e}"))?;
        env::set(&mut e, "VIBEKE_ISOLATION", "sandbox");
        match self.net {
            Net::Proxy(port) => {
                env::proxy_env(&mut e, port);
                env::set(&mut e, "VIBEKE_NETWORK", "allowlist");
            }
            Net::Open => env::set(&mut e, "VIBEKE_NETWORK", "open"),
            Net::None => env::set(&mut e, "VIBEKE_NETWORK", "none"),
        }
        let wrapped = if cfg!(target_os = "linux") {
            let bin = self
                .vibeke_bin
                .clone()
                .ok_or_else(|| "the vibeke binary is unknown".to_string())?;
            let mut v = vec![
                bin.to_string_lossy().into_owned(),
                "sandbox".into(),
                "bwrap".into(),
                "--policy".into(),
                profile.to_string_lossy().into_owned(),
                "--cwd".into(),
                cwd.to_string_lossy().into_owned(),
                "--".into(),
            ];
            v.extend(argv.iter().cloned());
            v
        } else {
            crate::seatbelt::wrap_argv(&profile, argv)
        };
        Ok(Prepared {
            argv: wrapped,
            env: e,
            profile,
            policy,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::HostVerdict;

    fn fixture() -> (tempfile::TempDir, NativeBox) {
        let t = tempfile::Builder::new()
            .prefix("vknb")
            .tempdir_in("/tmp")
            .unwrap();
        let r = t.path().canonicalize().unwrap();
        for d in [
            "h/notes",
            "s/plugins/state/a.b",
            "s/co/a.b",
            "c/plugins/a.b",
            "s/prof",
        ] {
            std::fs::create_dir_all(r.join(d)).unwrap();
        }
        let b = NativeBox {
            home: r.join("h"),
            plugin_root: r.join("s/co/a.b"),
            config_dir: r.join("c/plugins/a.b"),
            state_dir: r.join("s/plugins/state/a.b"),
            hidden: vec![r.join("s"), r.join("c")],
            extra_read: vec![],
            filesystem: resolve_filesystem(
                &["$HOME/notes".into(), "/opt/data:ro".into()],
                &r.join("h"),
                &r.join("s/plugins/state/a.b"),
                &r.join("c/plugins/a.b"),
            ),
            sockets: vec![],
            net: Net::None,
            vibeke_bin: Some(PathBuf::from("/usr/bin/true")),
            profile_dir: r.join("s/prof"),
        };
        (t, b)
    }

    #[test]
    fn profile_follows_the_capabilities() {
        let (_t, b) = fixture();
        let p = Policy::from_spec(&b.spec());
        assert!(p.allow_write.contains(&b.state_dir));
        assert!(
            p.allow_write.contains(&b.home.join("notes")),
            "declared rw path"
        );
        assert!(
            !p.allow_write.contains(&b.plugin_root),
            "plugin dir read-only"
        );
        assert!(
            !p.allow_write.iter().any(|w| w.ends_with("opt/data")),
            ":ro stays read-only"
        );
        assert!(p.network.is_none() && !p.network_open);
        let mut open = b.clone();
        open.net = Net::Proxy(4567);
        let p = Policy::from_spec(&open.spec());
        assert_eq!(p.network.as_ref().map(|n| n.0), Some(4567));
    }

    #[test]
    fn filesystem_entries_and_net_modes() {
        let fs = resolve_filesystem(
            &[
                "$PLUGIN_DATA".into(),
                "$PLUGIN_CONFIG/x:ro".into(),
                "relative".into(),
                "/a/../b".into(),
            ],
            Path::new("/h"),
            Path::new("/d"),
            Path::new("/c"),
        );
        assert_eq!(
            fs,
            vec![(PathBuf::from("/d"), true), (PathBuf::from("/c/x"), false)]
        );
        assert_eq!(net_for(&[], Some(1)), Net::None);
        assert_eq!(net_for(&["*".into()], None), Net::Open);
        assert_eq!(net_for(&["a.com".into()], Some(9)), Net::Proxy(9));
        assert_eq!(
            net_for(&["a.com".into()], None),
            Net::None,
            "no proxy, no network"
        );
    }

    #[test]
    fn egress_allows_exactly_the_declared_hosts() {
        let p = egress_policy(&["api.github.com".into()]);
        assert!(matches!(
            p.check_host("api.github.com", 443),
            HostVerdict::Allow { .. }
        ));
        assert!(!matches!(
            p.check_host("api.github.com", 22),
            HostVerdict::Allow { .. }
        ));
        assert!(matches!(p.check_host("example.com", 443), HostVerdict::Ask));
        // The base profile's harness APIs are not granted implicitly.
        let first = HARNESS_APIS[0].trim_start_matches("*.");
        assert!(!matches!(
            p.check_host(first, 443),
            HostVerdict::Allow { .. }
        ));
        // …unless declared.
        let q = egress_policy(&[first.to_string()]);
        assert!(matches!(
            q.check_host(first, 443),
            HostVerdict::Allow { .. }
        ));
    }

    #[test]
    fn limits_are_optional() {
        assert!(ResourceLimits::default().is_empty());
        assert!(ResourceLimits::default().apply().is_ok());
    }
}
