//! `container` level groundwork (13 §2.1, §4): provider detection and the `run` command line.
//!
//! What exists: detection of Apple `container`, OrbStack, Docker and Podman; a [`ContainerRunner`]
//! that wraps the pane command in `<runtime> run --rm -it …` with the checkout bind-mounted,
//! capabilities dropped, `no-new-privileges`, a pids limit and secrets passed by *name*
//! (`-e KEY`, value taken from the runtime CLI's env, never argv). The holder still runs on the
//! host and owns the PTY.
//!
//! Not yet: the in-box bridge/holder (13 §4), `clone` code isolation + `task sync`, images and
//! devcontainers (§9), and egress enforcement through the proxy — a bridge network has a default
//! route, so only network `none` (no network) and `open` (unfiltered, logged as such) are
//! accepted; proxy profiles return [`RunnerError::Unsupported`].

use crate::net::NetworkProfile;
use crate::runner::{Mount, PreparedSpawn, Runner, RunnerError, SpawnRequest};
use std::path::{Path, PathBuf};
use vk_proto::model::IsolationLevel;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    /// Apple `container` (one lightweight VM per container).
    AppleContainer(PathBuf),
    OrbStack(PathBuf),
    Docker(PathBuf),
    Podman(PathBuf),
}

impl Provider {
    pub fn name(&self) -> &'static str {
        match self {
            Provider::AppleContainer(_) => "apple-container",
            Provider::OrbStack(_) => "orbstack",
            Provider::Docker(_) => "docker",
            Provider::Podman(_) => "podman",
        }
    }
    /// The CLI that accepts docker-style `run` arguments.
    pub fn cli(&self) -> &Path {
        match self {
            Provider::AppleContainer(p)
            | Provider::OrbStack(p)
            | Provider::Docker(p)
            | Provider::Podman(p) => p,
        }
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .chain(["/usr/local/bin".into(), "/opt/homebrew/bin".into()])
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Detect a runtime without starting anything (13 §2.1 preference order per OS). OrbStack
/// ships a `docker` CLI; it is reported as OrbStack when `orb` is present.
pub fn detect() -> Option<Provider> {
    if cfg!(target_os = "macos")
        && let Some(p) = which("container")
    {
        return Some(Provider::AppleContainer(p));
    }
    if cfg!(target_os = "linux")
        && let Some(p) = which("podman")
    {
        return Some(Provider::Podman(p));
    }
    if let Some(d) = which("docker") {
        if which("orb").is_some() {
            return Some(Provider::OrbStack(d));
        }
        return Some(Provider::Docker(d));
    }
    which("podman").map(Provider::Podman)
}

pub struct ContainerRunner {
    pub provider: Provider,
    pub image: String,
    pub network: NetworkProfile,
    pub checkout: PathBuf,
    /// Extra read-only mounts (inbox at `/vibeke/inbox`, projected credential homes).
    pub mounts: Vec<Mount>,
    /// Env names whose values come from the CLI's own env (secrets stay out of argv).
    pub pass_env: Vec<String>,
    pub cpus: Option<String>,
    pub memory: Option<String>,
}

impl ContainerRunner {
    /// `<runtime> run …` argv for `cmd` (pure; unit-tested).
    pub fn run_argv(
        &self,
        name: &str,
        cwd: &Path,
        cmd: &[String],
    ) -> Result<Vec<String>, RunnerError> {
        let net = match self.network {
            NetworkProfile::None => "none",
            NetworkProfile::Open => "bridge",
            p => {
                return Err(RunnerError::Unsupported(format!(
                    "container egress filtering (network profile {}) is not implemented yet; use --network none|open or --isolate sandbox",
                    p.as_str()
                )));
            }
        };
        let s = |p: &Path| p.to_string_lossy().into_owned();
        let mut v = vec![
            s(self.provider.cli()),
            "run".into(),
            "--rm".into(),
            "-i".into(),
            "-t".into(),
            "--name".into(),
            name.to_string(),
            "--network".into(),
            net.into(),
        ];
        if !matches!(self.provider, Provider::AppleContainer(_)) {
            v.extend([
                "--cap-drop".into(),
                "ALL".into(),
                "--security-opt".into(),
                "no-new-privileges".into(),
                "--pids-limit".into(),
                "1024".into(),
            ]);
        }
        if let Some(c) = &self.cpus {
            v.extend(["--cpus".into(), c.clone()]);
        }
        if let Some(m) = &self.memory {
            v.extend(["--memory".into(), m.clone()]);
        }
        v.extend([
            "--volume".into(),
            format!("{}:{}", s(&self.checkout), s(&self.checkout)),
        ]);
        for m in &self.mounts {
            v.extend([
                "--volume".into(),
                format!(
                    "{}:{}{}",
                    s(&m.host),
                    s(&m.target),
                    if m.read_only { ":ro" } else { "" }
                ),
            ]);
        }
        for k in &self.pass_env {
            v.extend(["--env".into(), k.clone()]);
        }
        v.extend(["--workdir".into(), s(cwd), self.image.clone()]);
        v.extend(cmd.iter().cloned());
        Ok(v)
    }
}

impl Runner for ContainerRunner {
    fn level(&self) -> IsolationLevel {
        IsolationLevel::Container
    }
    fn provider(&self) -> &'static str {
        self.provider.name()
    }
    fn check(&self) -> Result<(), RunnerError> {
        if self.provider.cli().is_file() {
            Ok(())
        } else {
            Err(RunnerError::Unavailable {
                level: "container",
                reason: format!("{} not found", self.provider.cli().display()),
            })
        }
    }
    fn prepare(&self, req: SpawnRequest) -> Result<PreparedSpawn, RunnerError> {
        self.check()?;
        let name = format!("vk-{}", crate::runner::short_id(&req.pane_id));
        let argv = self.run_argv(&name, &req.cwd, &req.argv)?;
        let mut env = crate::env::scrub(&req.env);
        for k in &self.pass_env {
            if let Some((_, v)) = req.env.iter().find(|(x, _)| x == k) {
                crate::env::set(&mut env, k, v.clone());
            }
        }
        Ok(PreparedSpawn {
            argv,
            cwd: req.cwd,
            env,
            mounts: self.mounts.clone(),
            profile: None,
            broker_socket: None,
            visible_roots: vec![self.checkout.to_string_lossy().into_owned()],
            policy: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(net: NetworkProfile) -> ContainerRunner {
        ContainerRunner {
            provider: Provider::Docker("/usr/local/bin/docker".into()),
            image: "alpine:3".into(),
            network: net,
            checkout: "/work/co".into(),
            mounts: vec![Mount {
                host: "/state/inbox".into(),
                target: "/vibeke/inbox".into(),
                read_only: true,
            }],
            pass_env: vec!["CLAUDE_CODE_OAUTH_TOKEN".into()],
            cpus: Some("4".into()),
            memory: None,
        }
    }

    #[test]
    fn run_argv_shape() {
        let a = runner(NetworkProfile::None)
            .run_argv("vk-x", Path::new("/work/co"), &["sh".into(), "-l".into()])
            .unwrap();
        let j = a.join(" ");
        assert!(j.starts_with("/usr/local/bin/docker run --rm -i -t --name vk-x --network none"));
        assert!(j.contains("--cap-drop ALL"));
        assert!(j.contains("--volume /work/co:/work/co"));
        assert!(j.contains("--volume /state/inbox:/vibeke/inbox:ro"));
        // Secrets by name only.
        assert!(j.contains("--env CLAUDE_CODE_OAUTH_TOKEN "));
        assert!(j.ends_with("--workdir /work/co alpine:3 sh -l"));
    }

    #[test]
    fn proxy_profiles_are_refused_until_enforced() {
        assert!(
            runner(NetworkProfile::Dev)
                .run_argv("vk-x", Path::new("/"), &[])
                .is_err()
        );
        assert!(
            runner(NetworkProfile::Open)
                .run_argv("vk-x", Path::new("/"), &[])
                .is_ok()
        );
    }

    #[test]
    fn detection_does_not_start_anything() {
        // Only looks at PATH; fine whether or not a runtime is installed.
        let _ = detect();
    }

    /// Real container run, opt-in: `VIBEKE_CONTAINER_TESTS=1` (may pull `alpine:3`).
    #[test]
    fn real_container_run_gated() {
        if std::env::var("VIBEKE_CONTAINER_TESTS").as_deref() != Ok("1") {
            return;
        }
        let Some(p) = detect() else { return };
        let t = tempfile::tempdir().unwrap();
        let co = t.path().canonicalize().unwrap();
        let r = ContainerRunner {
            provider: p,
            image: "alpine:3".into(),
            network: NetworkProfile::None,
            checkout: co.clone(),
            mounts: vec![],
            pass_env: vec![],
            cpus: None,
            memory: None,
        };
        let mut argv = r
            .run_argv(
                "vk-test",
                &co,
                &["sh".into(), "-c".into(), "echo hi > out.txt".into()],
            )
            .unwrap();
        argv.retain(|a| a != "-t");
        let st = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .unwrap();
        assert!(st.success());
        assert_eq!(std::fs::read_to_string(co.join("out.txt")).unwrap(), "hi\n");
    }
}
