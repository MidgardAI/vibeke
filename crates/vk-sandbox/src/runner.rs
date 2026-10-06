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
//! | `vm` | [`VmRunner`] | placeholder (M4) |

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

impl SandboxRunner {
    pub fn pane_dir(&self, pane_id: &str) -> PathBuf {
        self.setup.root.join(short_id(pane_id))
    }

    fn spec(&self, private: &Path, broker: Option<&Path>) -> SandboxSpec {
        let s = &self.setup;
        let mut extra_read = s.extra_read.clone();
        extra_read.extend(s.projection.read.iter().cloned());
        if let Some(b) = &s.vibeke_bin {
            extra_read.push(b.clone());
        }
        let mut extra_write = s.extra_write.clone();
        extra_write.extend(s.projection.write.iter().cloned());
        SandboxSpec {
            home: s.home.clone(),
            checkout: s.checkout.clone(),
            git: s.git.clone(),
            private_dir: private.to_path_buf(),
            extra_read,
            extra_write,
            read_only_files: s.projection.read_only_files.clone(),
            hidden: s.hidden.clone(),
            home_read: s.home_read.clone(),
            unix_sockets: broker.map(|b| vec![b.to_path_buf()]).unwrap_or_default(),
            network: crate::policy::net_mode(s.network, s.proxy_port, &s.local_ports),
            allow_bind_localhost: true,
        }
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

    /// Write the per-pane profile and return (profile path, policy).
    pub fn write_profile(
        &self,
        private: &Path,
        broker: Option<&Path>,
    ) -> std::io::Result<(PathBuf, Policy)> {
        let policy = Policy::from_spec(&self.spec(private, broker));
        let path = if cfg!(target_os = "linux") {
            let p = private.join("policy.json");
            std::fs::write(&p, serde_json::to_vec_pretty(&policy).unwrap_or_default())?;
            p
        } else {
            let p = private.join("profile.sb");
            std::fs::write(&p, crate::seatbelt::render(&policy))?;
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
        let private = self.pane_dir(&req.pane_id);
        std::fs::create_dir_all(&private)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700))?;
        let broker = self.setup.broker.then(|| private.join("b.sock"));
        let (profile, policy) = self.write_profile(&private, broker.as_deref())?;
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
            reason: "the vm level ships in M4 (Lima vz/Tart on macOS, Firecracker on Linux)".into(),
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
        (IsolationLevel::Vm, Err("M4".into())),
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
        assert!(p.profile.as_ref().unwrap().is_file());
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
