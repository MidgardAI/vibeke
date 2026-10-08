//! Runner abstraction (05 §14, 13 §4): turn a pane/agent spawn request into the concrete
//! command the holder starts. Holders keep owning the PTY on the host for every level
//! implemented so far; the runner only wraps the child (argv wrapper, scrubbed env, cwd
//! mapping, mounts).
//!
//! | Level | Runner | Status |
//! |---|---|---|
//! | `host` | [`HostRunner`] | identity |
//! | `sandbox` | [`SandboxRunner`] | Seatbelt (macOS, tested); bubblewrap+Landlock+seccomp (Linux, generated + unit-tested, unverified on a Linux host) |
//! | `container` | [`crate::container::ContainerRunner`] | per-task box (`run -d`), panes are `exec -it` (Docker/OrbStack/Podman; Apple `container` open network only) |
//! | `vm` | [`VmRunner`] (unconfigured placeholder), [`crate::vm::VmBoxRunner`] | scaffolding behind `[isolation.vm] enabled`: fake backend tested, Lima/Tart command lines unverified on real VMs |

use crate::creds::Projection;
use crate::env;
use crate::net::NetworkProfile;
use crate::policy::{GitLayout, Policy, SandboxSpec};
use std::path::{Path, PathBuf};
use vk_proto::model::IsolationLevel;

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("{level} isolation is not available here: {reason}")]
    Unavailable { level: &'static str, reason: String },
    #[error("{0}")]
    Unsupported(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// What the server would spawn on the host.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub pane_id: String,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// The env a host pane would get (already carrying Vibeke's pane identity).
    pub env: Vec<(String, String)>,
}

/// A bind mount (container/vm runners; the sandbox runner works on host paths).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    pub target: PathBuf,
    pub read_only: bool,
}

/// What the holder actually starts.
#[derive(Debug, Clone)]
pub struct PreparedSpawn {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    /// The generated profile (Seatbelt SBPL / bwrap spec), if any.
    pub profile: Option<PathBuf>,
    /// Per-pane broker socket path the server must listen on (13 §4.1).
    pub broker_socket: Option<PathBuf>,
    /// Paths the contained tree can read without translation (06 A11.4).
    pub visible_roots: Vec<String>,
    pub policy: Option<Policy>,
}

pub trait Runner: Send + Sync {
    fn level(&self) -> IsolationLevel;
    fn provider(&self) -> &'static str;
    /// `Ok` when the level can run here (`vibeke doctor`, 13 §11).
    fn check(&self) -> Result<(), RunnerError>;
    fn prepare(&self, req: SpawnRequest) -> Result<PreparedSpawn, RunnerError>;
}

pub struct HostRunner;

impl Runner for HostRunner {
    fn level(&self) -> IsolationLevel {
        IsolationLevel::Host
    }
    fn provider(&self) -> &'static str {
        ""
    }
    fn check(&self) -> Result<(), RunnerError> {
        Ok(())
    }
    fn prepare(&self, req: SpawnRequest) -> Result<PreparedSpawn, RunnerError> {
        Ok(PreparedSpawn {
            argv: req.argv,
            cwd: req.cwd,
            env: req.env,
            mounts: vec![],
            profile: None,
            broker_socket: None,
            visible_roots: vec![],
            policy: None,
        })
    }
}

/// Everything a sandboxed task shares across its panes.
#[derive(Debug, Clone)]
pub struct SandboxSetup {
    pub home: PathBuf,
    pub checkout: PathBuf,
    pub git: Option<GitLayout>,
    /// `<state>/sbx/<task>`: per-pane private dirs are created below it.
    pub root: PathBuf,
    pub network: NetworkProfile,
    pub proxy_port: Option<u16>,
    /// Loopback ports the box may reach directly (the task's port lease, for previews/tests).
    pub local_ports: Vec<u16>,
    pub extra_read: Vec<PathBuf>,
    pub extra_write: Vec<PathBuf>,
    pub hidden: Vec<PathBuf>,
    pub home_read: Option<Vec<String>>,
    pub projection: Projection,
    /// The `vibeke` binary (hooks and the Linux helper run it inside the box).
    pub vibeke_bin: Option<PathBuf>,
    /// Host path of the egress proxy's unix socket (Linux net namespaces).
    pub egress_socket: Option<PathBuf>,
    pub broker: bool,
    /// Git-executed files inside the checkout ([`crate::gitexec::exec_targets`]): never
    /// writable.
    pub protected: Vec<PathBuf>,
}

pub struct SandboxRunner {
    pub setup: SandboxSetup,
}

/// Short, path-safe id for per-pane dirs (unix socket paths are limited to ~104 bytes).
pub fn short_id(id: &str) -> String {
    let s: String = id
        .chars()
        .rev()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    s.chars().rev().collect::<String>().to_ascii_lowercase()
}

/// Name of the control directory under a sandbox root (generated profiles/policies and exec
/// specs). Pane ids are alphanumeric ([`short_id`]), so it cannot collide with a pane dir.
pub const CONTROL_DIR: &str = ".ctl";

impl SandboxRunner {
    pub fn pane_dir(&self, pane_id: &str) -> PathBuf {
        self.setup.root.join(short_id(pane_id))
    }

    /// `<root>/.ctl`: never readable or writable from inside the box (see
    /// [`SandboxSpec::control_dir`]). The pane private dir is writable from inside, so a
    /// profile or exec spec there could be rewritten by the box before the host reads it.
    pub fn control_root(&self) -> PathBuf {
        self.setup.root.join(CONTROL_DIR)
    }

    /// The pane's control dir (`<root>/.ctl/<pane>`), created (or verified) without following
    /// symlinks.
    pub fn ensure_control_dir(&self, pane_id: &str) -> std::io::Result<PathBuf> {
        if !self.setup.root.exists() {
            std::fs::create_dir_all(&self.setup.root)?;
        }
        crate::fsafe::ensure_dir_under(
            &self.setup.root,
            &Path::new(CONTROL_DIR).join(short_id(pane_id)),
        )
    }

    /// May a projected path become a grant? The box can write inside projected dirs, so it may
    /// have replaced one with a symlink to a host path since the projection: paths under the
    /// sandbox root must be physically inside it (no symlink in any component); host paths
    /// (omp's session dir) must not be a symlink themselves. Never re-canonicalize a grant
    /// through a symlink.
    pub fn grantable(&self, p: &Path) -> bool {
        let root = crate::policy::canon(&self.setup.root);
        let lexical = if p.starts_with(&self.setup.root) {
            p.strip_prefix(&self.setup.root)
                .map(|r| root.join(r))
                .unwrap_or_else(|_| p.to_path_buf())
        } else {
            p.to_path_buf()
        };
        let ok = if lexical.starts_with(&root) {
            crate::fsafe::contained_no_symlink(&root, &lexical)
        } else {
            crate::fsafe::final_component_real(p)
        };
        if !ok {
            tracing::warn!(path = %p.display(), "projected path is a symlink or escapes the sandbox root; not granted");
        }
        ok
    }

    fn spec(&self, private: &Path, broker: Option<&Path>) -> SandboxSpec {
        let s = &self.setup;
        let mut extra_read = s.extra_read.clone();
        extra_read.extend(
            s.projection
                .read
                .iter()
                .filter(|p| self.grantable(p))
                .cloned(),
        );
        if let Some(b) = &s.vibeke_bin {
            extra_read.push(b.clone());
        }
        let mut extra_write = s.extra_write.clone();
        extra_write.extend(
            s.projection
                .write
                .iter()
                .filter(|p| self.grantable(p))
                .cloned(),
        );
        SandboxSpec {
            home: s.home.clone(),
            checkout: s.checkout.clone(),
            git: s.git.clone(),
            private_dir: private.to_path_buf(),
            extra_read,
            extra_write,
            read_only_files: s
                .projection
                .read_only_files
                .iter()
                .filter(|p| self.grantable(p))
                .cloned()
                .collect(),
            hidden: s.hidden.clone(),
            home_read: s.home_read.clone(),
            unix_sockets: broker.map(|b| vec![b.to_path_buf()]).unwrap_or_default(),
            network: crate::policy::net_mode(s.network, s.proxy_port, &s.local_ports),
            allow_bind_localhost: true,
            protected: s.protected.clone(),
            control_dir: Some(self.control_root()),
        }
    }

    /// The pane's private dir, created (or verified) without following symlinks.
    pub fn ensure_pane_dir(&self, pane_id: &str) -> std::io::Result<PathBuf> {
        if !self.setup.root.exists() {
            std::fs::create_dir_all(&self.setup.root)?;
        }
        crate::fsafe::ensure_dir_under(&self.setup.root, Path::new(&short_id(pane_id)))
    }

    /// Env for the contained tree: scrubbed host env + private dirs + proxy + credentials.
    pub fn env(
        &self,
        host_env: &[(String, String)],
        private: &Path,
        broker: Option<&Path>,
    ) -> std::io::Result<Vec<(String, String)>> {
        let mut e = env::scrub(host_env);
        env::private_dirs(&mut e, private)?;
        if let (true, Some(port)) = (self.setup.network.uses_proxy(), self.setup.proxy_port) {
            env::proxy_env(&mut e, port);
        }
        match broker {
            Some(b) => env::set(&mut e, "VIBEKE_SOCKET", b.to_string_lossy()),
            // No broker: point at a path that does not exist rather than the real socket.
            None => env::set(&mut e, "VIBEKE_SOCKET", "/nonexistent/vibeke-sandbox.sock"),
        }
        env::set(&mut e, "VIBEKE_ISOLATION", "sandbox");
        env::set(&mut e, "VIBEKE_NETWORK", self.setup.network.as_str());
        for (k, v) in &self.setup.projection.env {
            env::set(&mut e, k, v.clone());
        }
        Ok(e)
    }

    /// Write the per-pane profile into the pane's control dir `ctl` (never the box-writable
    /// private dir) and return (profile path, policy).
    pub fn write_profile(
        &self,
        ctl: &Path,
        private: &Path,
        broker: Option<&Path>,
    ) -> std::io::Result<(PathBuf, Policy)> {
        let policy = Policy::from_spec(&self.spec(private, broker));
        let path = if cfg!(target_os = "linux") {
            let p = ctl.join("policy.json");
            crate::fsafe::write_nofollow(
                &p,
                &serde_json::to_vec_pretty(&policy).unwrap_or_default(),
                0o600,
            )?;
            p
        } else {
            let p = ctl.join("profile.sb");
            crate::fsafe::write_nofollow(&p, crate::seatbelt::render(&policy).as_bytes(), 0o600)?;
            p
        };
        Ok((path, policy))
    }
}

impl Runner for SandboxRunner {
    fn level(&self) -> IsolationLevel {
        IsolationLevel::Sandbox
    }
    fn provider(&self) -> &'static str {
        if cfg!(target_os = "linux") {
            "bwrap"
        } else {
            "seatbelt"
        }
    }
    fn check(&self) -> Result<(), RunnerError> {
        if cfg!(target_os = "macos") {
            if crate::seatbelt::available() {
                return Ok(());
            }
            return Err(RunnerError::Unavailable {
                level: "sandbox",
                reason: format!("{} not found", crate::seatbelt::SANDBOX_EXEC),
            });
        }
        if cfg!(target_os = "linux") {
            if crate::linux::bwrap_available() {
                return Ok(());
            }
            return Err(RunnerError::Unavailable {
                level: "sandbox",
                reason: "bubblewrap (bwrap) not installed".into(),
            });
        }
        Err(RunnerError::Unavailable {
            level: "sandbox",
            reason: "unsupported OS".into(),
        })
    }
    fn prepare(&self, req: SpawnRequest) -> Result<PreparedSpawn, RunnerError> {
        self.check()?;
        let private = self.ensure_pane_dir(&req.pane_id)?;
        let ctl = self.ensure_control_dir(&req.pane_id)?;
        let broker = self.setup.broker.then(|| private.join("b.sock"));
        let (profile, policy) = self.write_profile(&ctl, &private, broker.as_deref())?;
        let env = self.env(&req.env, &private, broker.as_deref())?;
        let argv = if cfg!(target_os = "linux") {
            let bin = self
                .setup
                .vibeke_bin
                .clone()
                .ok_or_else(|| RunnerError::Unsupported("vibeke binary unknown".into()))?;
            let mut v = vec![
                bin.to_string_lossy().into_owned(),
                "sandbox".into(),
                "bwrap".into(),
                "--policy".into(),
                profile.to_string_lossy().into_owned(),
                "--cwd".into(),
                req.cwd.to_string_lossy().into_owned(),
            ];
            if let Some(s) = &self.setup.egress_socket {
                v.extend(["--egress".into(), s.to_string_lossy().into_owned()]);
            }
            if let Some(p) = self.setup.proxy_port {
                v.extend(["--proxy-port".into(), p.to_string()]);
            }
            v.push("--".into());
            v.extend(req.argv);
            v
        } else {
            crate::seatbelt::wrap_argv(&profile, &req.argv)
        };
        Ok(PreparedSpawn {
            argv,
            cwd: req.cwd,
            env,
            mounts: vec![],
            profile: Some(profile),
            broker_socket: broker,
            visible_roots: policy.visible_roots(),
            policy: Some(policy),
        })
    }
}

/// `vm` level placeholder (13 §13: M4).
pub struct VmRunner;

impl Runner for VmRunner {
    fn level(&self) -> IsolationLevel {
        IsolationLevel::Vm
    }
    fn provider(&self) -> &'static str {
        ""
    }
    fn check(&self) -> Result<(), RunnerError> {
        Err(RunnerError::Unavailable {
            level: "vm",
            reason: "the vm level is off: set `[isolation.vm] enabled = true` (providers: tart, lima on macOS; `fake` for dry runs)".into(),
        })
    }
    fn prepare(&self, _req: SpawnRequest) -> Result<PreparedSpawn, RunnerError> {
        Err(self.check().unwrap_err())
    }
}

/// Availability of every level for `vibeke doctor` (13 §11, acceptance 7).
pub fn availability() -> Vec<(IsolationLevel, Result<String, String>)> {
    let sbx = if crate::seatbelt::available() {
        Ok("seatbelt (sandbox-exec; deprecated by Apple but shipped)".to_string())
    } else if cfg!(target_os = "linux") && crate::linux::bwrap_available() {
        Ok("bubblewrap + Landlock + seccomp (unverified)".to_string())
    } else if cfg!(target_os = "linux") {
        Err("install bubblewrap (bwrap)".to_string())
    } else {
        Err("no sandbox-exec".to_string())
    };
    let ctr = match crate::container::select(None, crate::net::NetworkProfile::Dev) {
        Some(p) => Ok(format!(
            "{} at {} (per-task box; proxy profiles need the Linux vibeke binary)",
            p.name(),
            p.cli().display()
        )),
        None => {
            Err("no container runtime found (Apple container, OrbStack, Docker, Podman)".into())
        }
    };
    vec![
        (IsolationLevel::Host, Ok("always".into())),
        (IsolationLevel::Sandbox, sbx),
        (IsolationLevel::Container, ctr),
        (
            IsolationLevel::Vm,
            Err("off by default: set [isolation.vm] enabled = true (tart or lima)".into()),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_runner_is_identity() {
        let r = HostRunner
            .prepare(SpawnRequest {
                pane_id: "p".into(),
                argv: vec!["sh".into()],
                cwd: "/".into(),
                env: vec![("A".into(), "1".into())],
            })
            .unwrap();
        assert_eq!(r.argv, ["sh"]);
        assert_eq!(r.env, [("A".to_string(), "1".to_string())]);
    }

    #[test]
    fn vm_is_a_placeholder() {
        assert!(VmRunner.check().is_err());
    }

    #[test]
    fn short_ids() {
        assert_eq!(short_id("01JABCDEFGHJKMNPQRSTVWXYZ0"), "stvwxyz0");
        assert_eq!(short_id("a-b"), "ab");
    }

    fn setup_at(root: &Path) -> SandboxSetup {
        SandboxSetup {
            home: root.join("home"),
            checkout: root.join("home/co"),
            git: None,
            root: root.join("sbx"),
            network: NetworkProfile::None,
            proxy_port: None,
            local_ports: vec![],
            extra_read: vec![],
            extra_write: vec![],
            hidden: vec![],
            home_read: None,
            projection: Projection::default(),
            vibeke_bin: None,
            egress_socket: None,
            broker: false,
            protected: vec![],
        }
    }

    #[test]
    fn symlinked_projection_never_widens_grants() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("home/co")).unwrap();
        let mut s = setup_at(&root);
        std::fs::create_dir_all(&s.root).unwrap();
        let eph = crate::fsafe::ensure_dir_under(&s.root, Path::new("shared/home/claude")).unwrap();
        let cred = eph.join(".credentials.json");
        std::fs::write(&cred, "{}").unwrap();
        s.projection.write = vec![eph.clone()];
        s.projection.read_only_files = vec![cred.clone()];
        let r = SandboxRunner { setup: s };
        let private = r.ensure_pane_dir("01PANE0000000000000000AAAA").unwrap();
        let ctl = r.ensure_control_dir("01PANE0000000000000000AAAA").unwrap();
        let (_, pol) = r.write_profile(&ctl, &private, None).unwrap();
        assert!(pol.allow_write.contains(&eph));
        assert!(pol.allow_read_late.contains(&cred));
        // The box swaps its projected home (and credential file) for symlinks into the host.
        std::fs::remove_file(&cred).unwrap();
        std::os::unix::fs::symlink(root.join("home/.ssh-key"), &cred).unwrap();
        std::fs::remove_dir_all(&eph).unwrap();
        std::os::unix::fs::symlink(root.join("home"), &eph).unwrap();
        // The next pane (or a restart) must not grant the symlink targets.
        let private2 = r.ensure_pane_dir("01PANE0000000000000000BBBB").unwrap();
        let ctl2 = r.ensure_control_dir("01PANE0000000000000000BBBB").unwrap();
        let (_, pol) = r.write_profile(&ctl2, &private2, None).unwrap();
        assert!(!pol.allow_write.contains(&root.join("home")), "{pol:?}");
        assert!(!pol.allow_write.contains(&eph));
        assert!(!pol.allow_read_late.contains(&root.join("home")));
        assert!(!pol.allow_read_late.contains(&root.join("home/.ssh-key")));
        assert!(!pol.allow_read_late.contains(&cred));
        // A pane private dir replaced by a symlink is refused outright, and a planted profile
        // symlink is replaced rather than written through.
        let victim = root.join("home/victim");
        std::fs::write(&victim, "host").unwrap();
        let profile = ctl2.join(if cfg!(target_os = "linux") {
            "policy.json"
        } else {
            "profile.sb"
        });
        std::fs::remove_file(&profile).unwrap();
        std::os::unix::fs::symlink(&victim, &profile).unwrap();
        r.write_profile(&ctl2, &private2, None).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host");
        std::fs::remove_dir_all(&private2).unwrap();
        std::os::unix::fs::symlink(root.join("home"), &private2).unwrap();
        assert!(r.ensure_pane_dir("01PANE0000000000000000BBBB").is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandbox_runner_wraps_with_sandbox_exec() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("co")).unwrap();
        let r = SandboxRunner {
            setup: SandboxSetup {
                home: root.join("home"),
                checkout: root.join("co"),
                git: None,
                root: root.join("sbx"),
                network: NetworkProfile::Dev,
                proxy_port: Some(4123),
                local_ports: vec![],
                extra_read: vec![],
                extra_write: vec![],
                hidden: vec![],
                home_read: None,
                projection: Projection::default(),
                vibeke_bin: None,
                egress_socket: None,
                broker: true,
                protected: vec![],
            },
        };
        let p = r
            .prepare(SpawnRequest {
                pane_id: "01PANE0000000000000000ABCD".into(),
                argv: vec!["/bin/zsh".into(), "-l".into()],
                cwd: root.join("co"),
                env: vec![
                    ("PATH".into(), "/usr/bin".into()),
                    ("GITHUB_TOKEN".into(), "x".into()),
                ],
            })
            .unwrap();
        assert_eq!(p.argv[0], crate::seatbelt::SANDBOX_EXEC);
        assert_eq!(&p.argv[3..], ["/bin/zsh", "-l"]);
        let profile = p.profile.as_ref().unwrap();
        assert!(profile.is_file());
        // The profile sits in the control dir, outside the box-writable private dir, and the
        // policy hides and write-protects that dir.
        let ctl = r.control_root();
        assert!(profile.starts_with(&ctl), "{}", profile.display());
        assert!(!profile.starts_with(r.pane_dir("01PANE0000000000000000ABCD")));
        let pol = p.policy.as_ref().unwrap();
        let ctl_c = crate::policy::canon(&ctl);
        assert!(pol.never_read.contains(&ctl_c));
        assert!(pol.deny_write.contains(&ctl_c));
        assert!(!pol.can_read(profile));
        assert!(
            p.env
                .iter()
                .any(|(k, v)| k == "HTTPS_PROXY" && v.ends_with(":4123"))
        );
        assert!(!p.env.iter().any(|(k, _)| k == "GITHUB_TOKEN"));
        let sock = p.broker_socket.unwrap();
        assert!(
            p.env
                .iter()
                .any(|(k, v)| k == "VIBEKE_SOCKET" && v == &sock.to_string_lossy())
        );
    }
}
