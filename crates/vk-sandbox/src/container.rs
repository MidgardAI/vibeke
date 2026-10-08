//! The `container` level (13 §2.1, §4–§9): runtime providers, the long-lived per-task box and
//! the command lines that create, enter, stop and remove it.
//!
//! Design (deviations from 13 §4 are recorded in 13 §15):
//! - **One box per task**, created detached (`<runtime> run -d`) with a small init process as
//!   PID 1. Every pane is `<runtime> exec -it <box> <shell>`; the **holder stays on the host** and
//!   owns the PTY of that exec. Because the box outlives the server and the holders outlive it
//!   too, panes survive a host server restart without an in-box bridge.
//! - **Network**: `none` → `--network none`. Proxy profiles → also `--network none` (no route
//!   out at all); the server keeps one `<runtime> exec -i <box> vibeke sandbox bridge` stdio
//!   link per box (`vk_remote::boxlink`): the in-box end listens on `127.0.0.1:3128`, where
//!   `HTTP(S)_PROXY` points, and tunnels each connection to the host egress proxy. The same link
//!   carries the per-pane broker sockets. Bind-mounted unix sockets are not used: the macOS
//!   VM-backed runtimes do not forward them. This needs the static Linux `vibeke` binary mounted
//!   read-only at `/vibeke/bin/vibeke`. `open` → the runtime's default bridge network
//!   (unfiltered; reported as such).
//! - Hardening (Docker/OrbStack/Podman): `--cap-drop ALL`, `no-new-privileges`, a pids limit,
//!   host uid:gid (`--userns keep-id` on Podman), optional cpus/memory limits.
//! - Secrets are passed **per exec and by name** (`--env KEY`, the value comes from the runtime
//!   CLI's own env), never in argv and never in the container's persisted config.
//!
//! Apple `container` gets docker-style flags but only the `open` network: this build does not
//! know a network-less mode for it, so `none` and proxy profiles are refused (fail safe).

use crate::net::NetworkProfile;
use crate::runner::{PreparedSpawn, Runner, RunnerError, SpawnRequest};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use vk_proto::model::IsolationLevel;

/// In-box paths (fixed so sessions, hooks and pastes can rely on them).
pub const BOX_WORKSPACE: &str = "/workspace";
pub const BOX_INBOX: &str = "/vibeke/inbox";
/// In-box dir of the per-pane broker sockets (created by the in-box link end).
pub const BOX_BROKERS: &str = "/tmp/vibeke-brokers";
pub const BOX_BIN: &str = "/vibeke/bin/vibeke";
pub const BOX_CREDS: &str = "/vibeke/creds";
pub const BOX_HOME: &str = "/vibeke/home";
/// Port the in-box link end listens on (loopback inside the box's own net namespace).
pub const BOX_PROXY_PORT: u16 = 3128;

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
    /// The runtime CLI.
    pub fn cli(&self) -> &Path {
        match self {
            Provider::AppleContainer(p)
            | Provider::OrbStack(p)
            | Provider::Docker(p)
            | Provider::Podman(p) => p,
        }
    }
    pub(crate) fn s(&self) -> String {
        self.cli().to_string_lossy().into_owned()
    }
    pub(crate) fn apple(&self) -> bool {
        matches!(self, Provider::AppleContainer(_))
    }
    /// Build a provider from a config value: a provider name (`docker`, `orbstack`, `podman`,
    /// `apple-container`) resolved on PATH, or an absolute path to a docker-compatible CLI.
    pub fn from_config(v: &str) -> Option<Provider> {
        let p = Path::new(v);
        if p.is_absolute() {
            let base = p.file_name()?.to_string_lossy().to_string();
            return Some(match base.as_str() {
                "podman" => Provider::Podman(p.into()),
                "container" => Provider::AppleContainer(p.into()),
                _ => Provider::Docker(p.into()),
            });
        }
        match v {
            "docker" => which("docker").map(Provider::Docker),
            "orbstack" => which("docker").map(Provider::OrbStack),
            "podman" => which("podman").map(Provider::Podman),
            "apple-container" | "container" => which("container").map(Provider::AppleContainer),
            _ => None,
        }
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
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

/// The preferred provider: Apple `container` is detected first on macOS, but its network
/// isolation is not wired (see module docs), so a Docker-compatible runtime wins when the
/// network profile needs one.
pub fn select(config: Option<&str>, network: NetworkProfile) -> Option<Provider> {
    if let Some(c) = config {
        return Provider::from_config(c);
    }
    match detect() {
        Some(Provider::AppleContainer(p)) if network != NetworkProfile::Open => {
            Provider::from_config("orbstack")
                .filter(|_| which("orb").is_some())
                .or_else(|| Provider::from_config("docker"))
                .or_else(|| Provider::from_config("podman"))
                .or(Some(Provider::AppleContainer(p)))
        }
        other => other,
    }
}

/// Docker Sandboxes (`docker sandbox`, microVM-backed): detection only (13 §2.1). Looks for the
/// CLI plugin file; nothing is executed.
pub fn detect_docker_sandboxes(home: &Path) -> Option<PathBuf> {
    [
        home.join(".docker/cli-plugins/docker-sandbox"),
        PathBuf::from("/usr/local/lib/docker/cli-plugins/docker-sandbox"),
        PathBuf::from("/Applications/Docker.app/Contents/Resources/cli-plugins/docker-sandbox"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// Network of a box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxNet {
    /// `--network none`: loopback only.
    None,
    /// `--network none` + the in-box end of the box link, tunnelling to the host egress proxy.
    Proxy,
    /// Runtime default bridge: unfiltered egress.
    Open,
}

impl BoxNet {
    pub fn for_profile(p: NetworkProfile) -> BoxNet {
        match p {
            NetworkProfile::None => BoxNet::None,
            NetworkProfile::Open => BoxNet::Open,
            _ => BoxNet::Proxy,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoxMount {
    Bind {
        host: PathBuf,
        target: String,
        read_only: bool,
    },
    /// A named runtime volume (package caches shared across tasks, 13 §5).
    Volume {
        name: String,
        target: String,
    },
    Tmpfs {
        target: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Limits {
    pub cpus: Option<String>,
    pub memory: Option<String>,
    pub pids: Option<u32>,
}

/// Everything needed to create one box (pure data; argv generation is unit-tested).
#[derive(Debug, Clone)]
pub struct BoxSpec {
    pub provider: Provider,
    pub name: String,
    pub image: String,
    pub labels: Vec<(String, String)>,
    pub net: BoxNet,
    /// Working directory inside the box (`/workspace` for clones, the checkout path for binds).
    pub workdir: String,
    pub mounts: Vec<BoxMount>,
    /// Non-secret env set on the container itself.
    pub env: Vec<(String, String)>,
    /// `uid:gid` or a user name (devcontainer `remoteUser`).
    pub user: Option<String>,
    pub limits: Limits,
    /// The Linux `vibeke` binary is mounted at [`BOX_BIN`] (forwarder, hooks, broker client).
    pub in_box_vibeke: bool,
    /// Extra capabilities (user config only; `--cap-drop ALL` stays).
    pub cap_add: Vec<String>,
}

/// One `exec` into a box.
#[derive(Debug, Clone, Default)]
pub struct ExecOpts {
    pub tty: bool,
    pub interactive: bool,
    pub workdir: Option<String>,
    /// Non-secret env (`--env K=V`).
    pub env: Vec<(String, String)>,
    /// Secret env by name (`--env K`; the value must be in the runtime CLI's env).
    pub pass_env: Vec<String>,
    pub user: Option<String>,
}

/// POSIX single-quote a word.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Is `name` a plain named volume (`[A-Za-z0-9][A-Za-z0-9_.-]*`, ≤ 128)? Docker treats a
/// `--volume` source containing `/` (or `.`/`~` prefixes) as a **host path**, so anything else
/// would be a bind mount in disguise.
pub fn safe_volume_name(name: &str) -> bool {
    let mut cs = name.chars();
    cs.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// PID 1 when the box has no Linux `vibeke` binary: sleep forever, exit on TERM.
pub const SLEEP_INIT: &str = "trap 'exit 0' TERM INT; while :; do sleep 3600 & wait $!; done";

impl BoxSpec {
    fn check_net(&self) -> Result<(), RunnerError> {
        if self.provider.apple() && self.net != BoxNet::Open {
            return Err(RunnerError::Unsupported(
                "Apple container: network isolation is not wired in this build (only --network open); use OrbStack, Docker or Podman for none/proxy profiles".into(),
            ));
        }
        if self.net == BoxNet::Proxy && !self.in_box_vibeke {
            return Err(RunnerError::Unsupported(
                "proxy network profiles in a container need the Linux vibeke binary for the in-box egress forwarder: set [isolation.container] vibeke_linux (or VIBEKE_ARTIFACT_DIR), or use --network none|open".into(),
            ));
        }
        Ok(())
    }

    /// Entrypoint and args of PID 1.
    pub fn init(&self) -> (String, Vec<String>) {
        if self.in_box_vibeke {
            (
                BOX_BIN.into(),
                vec!["sandbox".to_string(), "box-init".to_string()],
            )
        } else {
            ("/bin/sh".into(), vec!["-c".into(), SLEEP_INIT.into()])
        }
    }

    /// `<runtime> run -d …` (pure).
    pub fn create_argv(&self) -> Result<Vec<String>, RunnerError> {
        self.check_net()?;
        let apple = self.provider.apple();
        let mut v = vec![self.provider.s(), "run".into(), "--detach".into()];
        v.extend(["--name".into(), self.name.clone()]);
        for (k, val) in &self.labels {
            v.extend(["--label".into(), format!("{k}={val}")]);
        }
        if !apple {
            let net = match self.net {
                BoxNet::None | BoxNet::Proxy => "none",
                BoxNet::Open => "bridge",
            };
            v.extend(["--network".into(), net.into()]);
            v.extend([
                "--cap-drop".into(),
                "ALL".into(),
                "--security-opt".into(),
                "no-new-privileges".into(),
                "--pids-limit".into(),
                self.limits.pids.unwrap_or(1024).to_string(),
            ]);
            for c in &self.cap_add {
                v.extend(["--cap-add".into(), c.clone()]);
            }
        }
        if let Some(c) = &self.limits.cpus {
            v.extend(["--cpus".into(), c.clone()]);
        }
        if let Some(m) = &self.limits.memory {
            v.extend(["--memory".into(), m.clone()]);
        }
        if let Some(u) = &self.user {
            v.extend(["--user".into(), u.clone()]);
        }
        if matches!(self.provider, Provider::Podman(_))
            && self
                .user
                .as_deref()
                .is_some_and(|u| u.chars().next().is_some_and(|c| c.is_ascii_digit()))
        {
            v.extend(["--userns".into(), "keep-id".into()]);
        }
        for m in &self.mounts {
            match m {
                BoxMount::Bind { host, .. } if !host.is_absolute() => {
                    return Err(RunnerError::Unsupported(format!(
                        "bind mount source {} is not an absolute path",
                        host.display()
                    )));
                }
                BoxMount::Volume { name, .. } if !safe_volume_name(name) => {
                    return Err(RunnerError::Unsupported(format!(
                        "volume source {name:?} is not a plain volume name (it would be a host bind mount)"
                    )));
                }
                _ => {}
            }
            match m {
                BoxMount::Bind {
                    host,
                    target,
                    read_only,
                } => v.extend([
                    "--volume".into(),
                    format!(
                        "{}:{}{}",
                        host.to_string_lossy(),
                        target,
                        if *read_only { ":ro" } else { "" }
                    ),
                ]),
                BoxMount::Volume { name, target } => {
                    v.extend(["--volume".into(), format!("{name}:{target}")])
                }
                BoxMount::Tmpfs { target } => v.extend(["--tmpfs".into(), target.clone()]),
            }
        }
        for (k, val) in &self.env {
            v.extend(["--env".into(), format!("{k}={val}")]);
        }
        if self.net == BoxNet::Proxy {
            for (k, val) in proxy_env() {
                v.extend(["--env".into(), format!("{k}={val}")]);
            }
        }
        v.extend(["--workdir".into(), self.workdir.clone()]);
        let (entry, args) = self.init();
        v.extend(["--entrypoint".into(), entry, self.image.clone()]);
        v.extend(args);
        Ok(v)
    }

    /// `<runtime> exec …` (pure).
    pub fn exec_argv(&self, o: &ExecOpts, cmd: &[String]) -> Vec<String> {
        let mut v = vec![self.provider.s(), "exec".into()];
        if o.interactive {
            v.push("--interactive".into());
        }
        if o.tty {
            v.push("--tty".into());
        }
        if let Some(w) = &o.workdir {
            v.extend(["--workdir".into(), w.clone()]);
        }
        if let Some(u) = o.user.as_ref().or(self.user.as_ref()) {
            v.extend(["--user".into(), u.clone()]);
        }
        for (k, val) in &o.env {
            v.extend(["--env".into(), format!("{k}={val}")]);
        }
        for k in &o.pass_env {
            v.extend(["--env".into(), k.clone()]);
        }
        v.push(self.name.clone());
        v.extend(cmd.iter().cloned());
        v
    }

    pub fn start_argv(&self) -> Vec<String> {
        vec![self.provider.s(), "start".into(), self.name.clone()]
    }
    pub fn stop_argv(&self) -> Vec<String> {
        let mut v = vec![self.provider.s(), "stop".into()];
        if !self.provider.apple() {
            v.extend(["--time".into(), "5".into()]);
        }
        v.push(self.name.clone());
        v
    }
    pub fn rm_argv(&self) -> Vec<String> {
        if self.provider.apple() {
            vec![
                self.provider.s(),
                "delete".into(),
                "--force".into(),
                self.name.clone(),
            ]
        } else {
            vec![
                self.provider.s(),
                "rm".into(),
                "--force".into(),
                self.name.clone(),
            ]
        }
    }
    pub fn inspect_argv(&self) -> Vec<String> {
        if self.provider.apple() {
            vec![self.provider.s(), "inspect".into(), self.name.clone()]
        } else {
            vec![
                self.provider.s(),
                "inspect".into(),
                "--format".into(),
                "{{.State.Status}}".into(),
                self.name.clone(),
            ]
        }
    }
    pub fn logs_argv(&self, tail: u32) -> Vec<String> {
        vec![
            self.provider.s(),
            "logs".into(),
            "--tail".into(),
            tail.to_string(),
            self.name.clone(),
        ]
    }
}

/// Proxy env inside a box (every common client spelling).
pub fn proxy_env() -> Vec<(String, String)> {
    let mut e = Vec::new();
    crate::env::proxy_env(&mut e, BOX_PROXY_PORT);
    e
}

/// Where a box is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxState {
    Missing,
    Running,
    Stopped,
    /// Frozen by `pause` (idle suspend, 13 §11): processes kept, nothing runs.
    Paused,
    Other,
}

impl BoxState {
    pub fn as_str(&self) -> &'static str {
        match self {
            BoxState::Missing => "missing",
            BoxState::Running => "running",
            BoxState::Stopped => "stopped",
            BoxState::Paused => "paused",
            BoxState::Other => "unknown",
        }
    }
    /// From `inspect` output (docker `{{.State.Status}}` or Apple's JSON).
    pub fn parse(ok: bool, out: &str) -> BoxState {
        if !ok {
            return BoxState::Missing;
        }
        let t = out.trim().to_ascii_lowercase();
        if t.contains("paused") {
            BoxState::Paused
        } else if t.contains("running") {
            BoxState::Running
        } else if t.contains("exited") || t.contains("stopped") || t.contains("created") {
            BoxState::Stopped
        } else if t.is_empty() || t == "[]" {
            BoxState::Missing
        } else {
            BoxState::Other
        }
    }
}

/// Output of one runtime CLI call.
#[derive(Debug, Clone)]
pub struct CmdOut {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run a runtime command (blocking, with a timeout). `env` is the CLI's own env.
pub fn run_cmd(
    argv: &[String],
    env: &[(String, String)],
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> std::io::Result<CmdOut> {
    use std::io::{Read, Write};
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..])
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn()?;
    if let (Some(data), Some(mut si)) = (stdin, child.stdin.take()) {
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = si.write_all(&data);
        });
    }
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let ho = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = so.as_mut() {
            let _ = o.read_to_string(&mut s);
        }
        s
    });
    let he = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = se.as_mut() {
            let _ = o.read_to_string(&mut s);
        }
        s
    });
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("{} timed out after {}s", argv.join(" "), timeout.as_secs()),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Ok(CmdOut {
        ok: status.success(),
        stdout: ho.join().unwrap_or_default(),
        stderr: he.join().unwrap_or_default(),
    })
}

/// Host env the runtime CLI needs to find its daemon/context (never the user's secrets).
pub fn cli_env(host_env: &[(String, String)]) -> Vec<(String, String)> {
    host_env
        .iter()
        .filter(|(k, _)| {
            matches!(
                k.as_str(),
                "PATH" | "HOME" | "USER" | "LOGNAME" | "TMPDIR" | "LANG" | "XDG_RUNTIME_DIR"
            ) || k.starts_with("DOCKER_")
                || k.starts_with("CONTAINER_")
                || k.starts_with("CONTAINERS_")
                || k.starts_with("XDG_CONFIG")
                || k.starts_with("LC_")
        })
        .cloned()
        .collect()
}

/// Everything that is the same for every pane of one box.
#[derive(Debug, Clone)]
pub struct ContainerBox {
    pub spec: BoxSpec,
    /// Env for the runtime CLI (host basics, no secrets).
    pub cli_env: Vec<(String, String)>,
    /// Secrets the panes receive by name (value in the holder's env, 13 §8).
    pub secrets: Vec<(String, String)>,
    /// Non-secret per-exec env (credential dirs rewritten to box paths, identity, …).
    pub exec_env: Vec<(String, String)>,
    /// Host dir holding per-pane broker sockets (reached from the box through the link).
    pub run_dir: PathBuf,
    /// Host paths a box pane sees at the same path (worktree mode only).
    pub visible_roots: Vec<String>,
    /// Shell to start in a pane when the host asked for its own login shell.
    pub shell: String,
}

impl ContainerBox {
    pub fn state(&self) -> BoxState {
        match run_cmd(
            &self.spec.inspect_argv(),
            &self.cli_env,
            None,
            Duration::from_secs(15),
        ) {
            Ok(o) => BoxState::parse(o.ok, &o.stdout),
            Err(_) => BoxState::Other,
        }
    }

    /// Create or start the box (blocking). Returns `true` when it was created now.
    pub fn ensure_running(&self) -> Result<bool, RunnerError> {
        match self.state() {
            BoxState::Running => Ok(false),
            BoxState::Paused => {
                self.cli(&self.spec.unpause_argv(), Duration::from_secs(60))?;
                Ok(false)
            }
            BoxState::Stopped => {
                self.cli(&self.spec.start_argv(), Duration::from_secs(60))?;
                Ok(false)
            }
            BoxState::Missing => {
                let argv = self.spec.create_argv()?;
                self.cli(&argv, Duration::from_secs(120))?;
                Ok(true)
            }
            BoxState::Other => Err(RunnerError::Unsupported(format!(
                "container {} is in an unexpected state (see `{} inspect {}`)",
                self.spec.name,
                self.spec.provider.cli().display(),
                self.spec.name
            ))),
        }
    }

    pub(crate) fn cli(&self, argv: &[String], t: Duration) -> Result<CmdOut, RunnerError> {
        let o = run_cmd(argv, &self.cli_env, None, t)?;
        if !o.ok {
            return Err(RunnerError::Unsupported(format!(
                "{} {} failed: {}",
                self.spec.provider.name(),
                argv.get(1).map(String::as_str).unwrap_or(""),
                o.stderr.trim()
            )));
        }
        Ok(o)
    }

    pub fn stop(&self) -> Result<(), RunnerError> {
        match self.state() {
            BoxState::Running => {
                self.cli(&self.spec.stop_argv(), Duration::from_secs(60))?;
            }
            BoxState::Paused => {
                self.cli(&self.spec.unpause_argv(), Duration::from_secs(60))?;
                self.cli(&self.spec.stop_argv(), Duration::from_secs(60))?;
            }
            _ => {}
        }
        Ok(())
    }

    pub fn remove(&self) -> Result<(), RunnerError> {
        if self.state() != BoxState::Missing {
            self.cli(&self.spec.rm_argv(), Duration::from_secs(60))?;
        }
        Ok(())
    }

    /// Run a non-interactive shell script inside the box (clone init, setup, postCreate).
    pub fn exec_script(
        &self,
        script: &str,
        user: Option<&str>,
        timeout: Duration,
    ) -> std::io::Result<CmdOut> {
        let argv = self.spec.exec_argv(
            &ExecOpts {
                workdir: Some(self.spec.workdir.clone()),
                env: self.exec_env.clone(),
                user: user.map(str::to_string),
                ..Default::default()
            },
            &["/bin/sh".into(), "-c".into(), script.into()],
        );
        run_cmd(&argv, &self.cli_env, None, timeout)
    }

    /// The prefix git uses as `--upload-pack` / `--receive-pack` (runs inside the box).
    /// `<runtime> exec -i <box> /vibeke/bin/vibeke sandbox bridge …`: the stdio link carrying
    /// egress and broker channels (`vk_remote::boxlink`).
    pub fn link_argv(&self) -> Vec<String> {
        let mut cmd = vec![
            BOX_BIN.to_string(),
            "sandbox".into(),
            "bridge".into(),
            "--brokers".into(),
            BOX_BROKERS.into(),
        ];
        if self.spec.net == BoxNet::Proxy {
            cmd.extend(["--listen".into(), format!("127.0.0.1:{BOX_PROXY_PORT}")]);
        }
        self.spec.exec_argv(
            &ExecOpts {
                interactive: true,
                ..Default::default()
            },
            &cmd,
        )
    }

    pub fn git_service(&self, service: &str) -> String {
        let argv = self.spec.exec_argv(
            &ExecOpts {
                interactive: true,
                ..Default::default()
            },
            &["git".into(), service.into()],
        );
        argv.iter()
            .map(|a| sh_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Per-pane exec argv + the holder env (runtime CLI env + secret values for `--env KEY`).
    pub fn pane_exec(
        &self,
        broker: Option<&str>,
        identity: &[(String, String)],
        cmd: &[String],
    ) -> (Vec<String>, Vec<(String, String)>) {
        let mut env = self.exec_env.clone();
        for (k, v) in identity {
            crate::env::set(&mut env, k, v.clone());
        }
        crate::env::set(
            &mut env,
            "VIBEKE_SOCKET",
            broker.unwrap_or("/nonexistent/vibeke-box.sock"),
        );
        let argv = self.spec.exec_argv(
            &ExecOpts {
                tty: true,
                interactive: true,
                workdir: Some(self.spec.workdir.clone()),
                env,
                pass_env: self.secrets.iter().map(|(k, _)| k.clone()).collect(),
                user: None,
            },
            cmd,
        );
        let mut holder_env = self.cli_env.clone();
        for (k, v) in &self.secrets {
            crate::env::set(&mut holder_env, k, v.clone());
        }
        (argv, holder_env)
    }
}

/// Host shells don't exist in the box: a host login shell becomes `sh -l` (or `bash -l` when
/// asked for explicitly by config); any other command runs as given.
pub fn box_command(argv: &[String], shell: &str) -> Vec<String> {
    let is_host_shell = argv.len() <= 2
        && argv.first().is_some_and(|a| {
            let b = Path::new(a)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            matches!(b.as_str(), "zsh" | "bash" | "fish" | "sh" | "dash" | "ksh")
        })
        && argv.get(1).is_none_or(|a| a == "-l" || a == "--login");
    if argv.is_empty() || is_host_shell {
        vec![shell.to_string(), "-l".into()]
    } else {
        argv.to_vec()
    }
}

/// Rewrite the host inbox prefix in pasted text to the box inbox (06 A11.4). The TUI copies a
/// dropped file into the host inbox and pastes its host path; inside a box that path is
/// [`BOX_INBOX`]. Both the raw and the backslash-escaped spelling of the prefix are handled.
pub fn translate_inbox_paths(text: &str, host_inbox: &Path) -> String {
    let raw = host_inbox
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string();
    if raw.is_empty() {
        return text.to_string();
    }
    let mut out = text.replace(&format!("{raw}/"), &format!("{BOX_INBOX}/"));
    let escaped = raw.replace(' ', "\\ ");
    if escaped != raw {
        out = out.replace(&format!("{escaped}/"), &format!("{BOX_INBOX}/"));
    }
    out
}

/// Runner view of a box (13 §4): only builds the exec argv; the box must already be running.
pub struct ContainerRunner {
    pub b: ContainerBox,
}

impl Runner for ContainerRunner {
    fn level(&self) -> IsolationLevel {
        IsolationLevel::Container
    }
    fn provider(&self) -> &'static str {
        self.b.spec.provider.name()
    }
    fn check(&self) -> Result<(), RunnerError> {
        if !self.b.spec.provider.cli().is_file() {
            return Err(RunnerError::Unavailable {
                level: "container",
                reason: format!("{} not found", self.b.spec.provider.cli().display()),
            });
        }
        self.b.spec.check_net()
    }
    fn prepare(&self, req: SpawnRequest) -> Result<PreparedSpawn, RunnerError> {
        self.check()?;
        let short = crate::runner::short_id(&req.pane_id);
        let broker = format!("{BOX_BROKERS}/{short}.sock");
        let identity: Vec<(String, String)> = req
            .env
            .iter()
            .filter(|(k, _)| {
                matches!(
                    k.as_str(),
                    "TERM"
                        | "COLORTERM"
                        | "TERM_PROGRAM"
                        | "TERM_PROGRAM_VERSION"
                        | "LANG"
                        | "VIBEKE"
                        | "VIBEKE_PANE_ID"
                        | "VIBEKE_PANE_ULID"
                        | "VIBEKE_WORKSPACE_ID"
                        | "VIBEKE_TAB_ID"
                        | "VIBEKE_SESSION"
                ) || k.starts_with("LC_")
            })
            .cloned()
            .collect();
        let cmd = box_command(&req.argv, &self.b.shell);
        let (argv, env) = self.b.pane_exec(Some(&broker), &identity, &cmd);
        Ok(PreparedSpawn {
            argv,
            cwd: req.cwd,
            env,
            mounts: vec![],
            profile: None,
            broker_socket: Some(self.b.run_dir.join(format!("{short}.sock"))),
            visible_roots: self.b.visible_roots.clone(),
            policy: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(p: Provider, net: BoxNet, bin: bool) -> BoxSpec {
        BoxSpec {
            provider: p,
            name: "vk-abc".into(),
            image: "alpine:3.20".into(),
            labels: vec![("vibeke.task".into(), "T1".into())],
            net,
            workdir: BOX_WORKSPACE.into(),
            mounts: vec![
                BoxMount::Bind {
                    host: "/state/inbox".into(),
                    target: BOX_INBOX.into(),
                    read_only: true,
                },
                BoxMount::Bind {
                    host: "/state/sbx/x/workspace".into(),
                    target: BOX_WORKSPACE.into(),
                    read_only: false,
                },
                BoxMount::Volume {
                    name: "vk-cache-npm".into(),
                    target: "/vibeke/cache/npm".into(),
                },
            ],
            env: vec![("HOME".into(), BOX_HOME.into())],
            user: Some("501:20".into()),
            limits: Limits {
                cpus: Some("4".into()),
                memory: Some("8g".into()),
                pids: None,
            },
            in_box_vibeke: bin,
            cap_add: vec![],
        }
    }

    fn docker() -> Provider {
        Provider::Docker("/usr/local/bin/docker".into())
    }

    #[test]
    fn docker_create_argv_none() {
        let a = spec(docker(), BoxNet::None, false).create_argv().unwrap();
        let j = a.join(" ");
        assert!(j.starts_with("/usr/local/bin/docker run --detach --name vk-abc"));
        assert!(j.contains("--label vibeke.task=T1"));
        assert!(j.contains("--network none"));
        assert!(j.contains("--cap-drop ALL --security-opt no-new-privileges --pids-limit 1024"));
        assert!(j.contains("--cpus 4 --memory 8g --user 501:20"));
        assert!(j.contains("--volume /state/inbox:/vibeke/inbox:ro"));
        assert!(j.contains("--volume /state/sbx/x/workspace:/workspace "));
        assert!(j.contains("--volume vk-cache-npm:/vibeke/cache/npm"));
        assert!(!j.contains("HTTP_PROXY"));
        // No vibeke binary: a portable sleep loop is PID 1.
        assert!(j.contains("--workdir /workspace --entrypoint /bin/sh alpine:3.20 -c"));
        assert_eq!(a.last().unwrap(), SLEEP_INIT);
    }

    #[test]
    fn proxy_network_has_no_route_and_a_forwarder() {
        let a = spec(docker(), BoxNet::Proxy, true).create_argv().unwrap();
        let j = a.join(" ");
        // Still no network namespace route out: only the exec-stdio link crosses.
        assert!(j.contains("--network none"));
        assert!(j.contains("--env HTTPS_PROXY=http://127.0.0.1:3128"));
        assert!(j.contains("--env https_proxy=http://127.0.0.1:3128"));
        assert!(j.ends_with("--entrypoint /vibeke/bin/vibeke alpine:3.20 sandbox box-init"));
        // Without the in-box binary a proxy profile is refused rather than silently open.
        let e = spec(docker(), BoxNet::Proxy, false)
            .create_argv()
            .unwrap_err();
        assert!(e.to_string().contains("vibeke_linux"), "{e}");
    }

    #[test]
    fn volume_sources_must_be_plain_names() {
        for ok in ["vk-cache-npm", "vibeke.cache_1", "a"] {
            assert!(safe_volume_name(ok), "{ok}");
        }
        for bad in [
            "/Users/alice",
            "/var/run/docker.sock",
            "./data",
            "../x",
            "~/x",
            ".hidden",
            "a/b",
            "",
            "x:y",
        ] {
            assert!(!safe_volume_name(bad), "{bad}");
            let mut s = spec(docker(), BoxNet::None, false);
            s.mounts.push(BoxMount::Volume {
                name: bad.into(),
                target: "/host".into(),
            });
            assert!(s.create_argv().is_err(), "{bad} reached argv");
        }
        let mut s = spec(docker(), BoxNet::None, false);
        s.mounts.push(BoxMount::Bind {
            host: "relative/dir".into(),
            target: "/x".into(),
            read_only: true,
        });
        assert!(s.create_argv().is_err());
    }

    #[test]
    fn open_network_uses_bridge() {
        let j = spec(docker(), BoxNet::Open, false)
            .create_argv()
            .unwrap()
            .join(" ");
        assert!(j.contains("--network bridge"));
    }

    #[test]
    fn podman_keeps_id() {
        let j = spec(
            Provider::Podman("/usr/bin/podman".into()),
            BoxNet::None,
            false,
        )
        .create_argv()
        .unwrap()
        .join(" ");
        assert!(j.starts_with("/usr/bin/podman run --detach"));
        assert!(j.contains("--user 501:20 --userns keep-id"));
        let mut s = spec(
            Provider::Podman("/usr/bin/podman".into()),
            BoxNet::None,
            false,
        );
        s.user = Some("vscode".into());
        assert!(!s.create_argv().unwrap().join(" ").contains("keep-id"));
    }

    #[test]
    fn orbstack_is_docker_compatible() {
        let j = spec(
            Provider::OrbStack("/usr/local/bin/docker".into()),
            BoxNet::None,
            false,
        )
        .create_argv()
        .unwrap()
        .join(" ");
        assert!(j.contains("--network none --cap-drop ALL"));
    }

    #[test]
    fn apple_container_only_open() {
        let p = Provider::AppleContainer("/usr/local/bin/container".into());
        assert!(spec(p.clone(), BoxNet::None, false).create_argv().is_err());
        assert!(spec(p.clone(), BoxNet::Proxy, true).create_argv().is_err());
        let s = spec(p, BoxNet::Open, false);
        let j = s.create_argv().unwrap().join(" ");
        assert!(j.starts_with("/usr/local/bin/container run --detach --name vk-abc"));
        assert!(!j.contains("--network"));
        assert!(!j.contains("--cap-drop"));
        assert_eq!(
            s.rm_argv(),
            ["/usr/local/bin/container", "delete", "--force", "vk-abc"]
        );
        assert_eq!(s.inspect_argv().len(), 3);
    }

    #[test]
    fn exec_passes_secrets_by_name() {
        let s = spec(docker(), BoxNet::None, false);
        let b = ContainerBox {
            spec: s,
            cli_env: vec![("PATH".into(), "/usr/bin".into())],
            secrets: vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "sk-FAKE-secret".into())],
            exec_env: vec![(
                "CLAUDE_CONFIG_DIR".into(),
                "/vibeke/creds/home/claude".into(),
            )],
            run_dir: "/state/sbx/x/run".into(),
            visible_roots: vec![],
            shell: "/bin/sh".into(),
        };
        let (argv, env) = b.pane_exec(
            Some("/tmp/vibeke-brokers/p1.sock"),
            &[("VIBEKE_PANE_ID".into(), "w1:p1".into())],
            &["/bin/sh".into(), "-l".into()],
        );
        let j = argv.join(" ");
        assert!(j.starts_with(
            "/usr/local/bin/docker exec --interactive --tty --workdir /workspace --user 501:20"
        ));
        assert!(j.contains("--env CLAUDE_CONFIG_DIR=/vibeke/creds/home/claude"));
        assert!(j.contains("--env VIBEKE_SOCKET=/tmp/vibeke-brokers/p1.sock"));
        assert!(j.contains("--env VIBEKE_PANE_ID=w1:p1"));
        assert!(j.contains("--env CLAUDE_CODE_OAUTH_TOKEN vk-abc /bin/sh -l"));
        assert!(!j.contains("sk-FAKE-secret"));
        // The value reaches the runtime CLI through the holder's env only.
        assert!(
            env.iter()
                .any(|(k, v)| k == "CLAUDE_CODE_OAUTH_TOKEN" && v == "sk-FAKE-secret")
        );
        assert!(
            b.git_service("upload-pack")
                .ends_with("vk-abc git upload-pack")
        );
        assert!(b.link_argv().join(" ").ends_with(
            "exec --interactive --user 501:20 vk-abc /vibeke/bin/vibeke sandbox bridge --brokers /tmp/vibeke-brokers"
        ));
        let mut p = b.clone();
        p.spec.net = BoxNet::Proxy;
        assert!(p.link_argv().join(" ").ends_with("--listen 127.0.0.1:3128"));
    }

    #[test]
    fn runner_prepare_shapes_a_pane() {
        let b = ContainerBox {
            spec: spec(Provider::Docker("/bin/sh".into()), BoxNet::None, false),
            cli_env: vec![],
            secrets: vec![],
            exec_env: vec![],
            run_dir: "/state/sbx/x/run".into(),
            visible_roots: vec![],
            shell: "/bin/sh".into(),
        };
        let r = ContainerRunner { b };
        let p = r
            .prepare(SpawnRequest {
                pane_id: "01PANE0000000000000000ABCD".into(),
                argv: vec!["/bin/zsh".into(), "-l".into()],
                cwd: "/host/co".into(),
                env: vec![
                    ("VIBEKE_PANE_TOKEN".into(), "tok".into()),
                    ("HOME".into(), "/Users/x".into()),
                    ("VIBEKE_PANE_ID".into(), "w1:p1".into()),
                ],
            })
            .unwrap();
        let j = p.argv.join(" ");
        assert!(j.ends_with("vk-abc /bin/sh -l"), "{j}");
        assert!(j.contains("VIBEKE_SOCKET=/tmp/vibeke-brokers/0000abcd.sock"));
        assert!(!j.contains("tok"));
        assert!(!j.contains("/Users/x"));
        assert_eq!(
            p.broker_socket.unwrap(),
            PathBuf::from("/state/sbx/x/run/0000abcd.sock")
        );
    }

    #[test]
    fn host_shells_become_box_shells() {
        assert_eq!(
            box_command(&["/bin/zsh".into(), "-l".into()], "/bin/sh"),
            ["/bin/sh", "-l"]
        );
        assert_eq!(box_command(&[], "/bin/bash"), ["/bin/bash", "-l"]);
        assert_eq!(
            box_command(&["claude".into(), "--resume".into()], "/bin/sh"),
            ["claude", "--resume"]
        );
    }

    #[test]
    fn box_state_parsing() {
        assert_eq!(BoxState::parse(false, ""), BoxState::Missing);
        assert_eq!(BoxState::parse(true, "running\n"), BoxState::Running);
        assert_eq!(BoxState::parse(true, "exited"), BoxState::Stopped);
        assert_eq!(BoxState::parse(true, "created"), BoxState::Stopped);
        assert_eq!(
            BoxState::parse(true, r#"[{"status":"stopped"}]"#),
            BoxState::Stopped
        );
    }

    #[test]
    fn inbox_translation() {
        let host = Path::new("/Users/e/.local/state/vibeke/inbox");
        assert_eq!(
            translate_inbox_paths(
                "/Users/e/.local/state/vibeke/inbox/3f9a1c0b2e7d/Shot\\ 1.png",
                host
            ),
            "/vibeke/inbox/3f9a1c0b2e7d/Shot\\ 1.png"
        );
        assert_eq!(
            translate_inbox_paths("'/Users/e/.local/state/vibeke/inbox/ab/x y.png'", host),
            "'/vibeke/inbox/ab/x y.png'"
        );
        let spaced = Path::new("/Users/e/My Stuff/inbox");
        assert_eq!(
            translate_inbox_paths("/Users/e/My\\ Stuff/inbox/ab/f.png", spaced),
            "/vibeke/inbox/ab/f.png"
        );
        assert_eq!(translate_inbox_paths("echo hi", host), "echo hi");
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("abc/d-e"), "abc/d-e");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
    }

    #[test]
    fn detection_does_not_start_anything() {
        let _ = detect();
        let _ = detect_docker_sandboxes(Path::new("/nonexistent"));
    }

    /// Removes a gated test's box even when an assertion fails.
    pub(crate) struct RemoveOnDrop(pub ContainerBox);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = self.0.remove();
        }
    }

    /// A locally present image for gated tests: never pulls.
    pub(crate) fn local_image(p: &Provider) -> Option<String> {
        let want = std::env::var("VIBEKE_CONTAINER_TEST_IMAGE").unwrap_or("alpine:3.20".into());
        let o = run_cmd(
            &[
                p.cli().to_string_lossy().into_owned(),
                "image".into(),
                "inspect".into(),
                want.clone(),
            ],
            &cli_env(&std::env::vars().collect::<Vec<_>>()),
            None,
            Duration::from_secs(20),
        )
        .ok()?;
        o.ok.then_some(want)
    }

    /// Real box lifecycle, opt-in: `VIBEKE_CONTAINER_TESTS=1` and a local image (default
    /// `alpine:3.20`, override with `VIBEKE_CONTAINER_TEST_IMAGE`). Never pulls.
    #[test]
    fn real_box_lifecycle_gated() {
        if std::env::var("VIBEKE_CONTAINER_TESTS").as_deref() != Ok("1") {
            return;
        }
        let Some(p) = select(None, NetworkProfile::None) else {
            eprintln!("no container runtime; skipping");
            return;
        };
        let Some(image) = local_image(&p) else {
            eprintln!("test image not present locally; not pulling; skipping");
            return;
        };
        let t = tempfile::tempdir().unwrap();
        let ws = t.path().canonicalize().unwrap().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let name = format!("vk-test-{}", std::process::id());
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let b = ContainerBox {
            spec: BoxSpec {
                provider: p,
                name: name.clone(),
                image,
                labels: vec![("vibeke.test".into(), "1".into())],
                net: BoxNet::None,
                workdir: BOX_WORKSPACE.into(),
                mounts: vec![BoxMount::Bind {
                    host: ws.clone(),
                    target: BOX_WORKSPACE.into(),
                    read_only: false,
                }],
                env: vec![("HOME".into(), "/tmp".into())],
                user: Some(format!("{uid}:{gid}")),
                limits: Limits::default(),
                in_box_vibeke: false,
                cap_add: vec![],
            },
            cli_env: cli_env(&std::env::vars().collect::<Vec<_>>()),
            secrets: vec![("VK_TEST_SECRET".into(), "s3cret-value".into())],
            exec_env: vec![],
            run_dir: t.path().join("run"),
            visible_roots: vec![],
            shell: "/bin/sh".into(),
        };
        let _ = b.remove();
        let _guard = RemoveOnDrop(b.clone());
        assert!(b.ensure_running().unwrap());
        assert_eq!(b.state(), BoxState::Running);
        assert!(!b.ensure_running().unwrap());
        let o = b
            .exec_script("echo hi > out.txt && id -u", None, Duration::from_secs(30))
            .unwrap();
        assert!(o.ok, "{}", o.stderr);
        assert_eq!(o.stdout.trim(), uid.to_string());
        assert_eq!(std::fs::read_to_string(ws.join("out.txt")).unwrap(), "hi\n");
        // Network none: no IPv4 route at all (tunnel devices may exist but are down).
        let o = b
            .exec_script(
                "tail -n +2 /proc/net/route | wc -l",
                None,
                Duration::from_secs(30),
            )
            .unwrap();
        assert_eq!(o.stdout.trim(), "0", "routes in a --network none box");
        // A secret passed by name reaches the exec'd process but not the container config.
        let (mut argv, env) = b.pane_exec(
            None,
            &[],
            &[
                "/bin/sh".into(),
                "-c".into(),
                "printf %s \"$VK_TEST_SECRET\"".into(),
            ],
        );
        argv.retain(|a| a != "--tty");
        let o = run_cmd(&argv, &env, None, Duration::from_secs(30)).unwrap();
        assert_eq!(o.stdout, "s3cret-value");
        let insp = run_cmd(
            &[
                b.spec.provider.cli().to_string_lossy().into_owned(),
                "inspect".into(),
                name.clone(),
            ],
            &b.cli_env,
            None,
            Duration::from_secs(20),
        )
        .unwrap();
        assert!(!insp.stdout.contains("s3cret-value"));
        b.stop().unwrap();
        assert_eq!(b.state(), BoxState::Stopped);
        assert!(!b.ensure_running().unwrap());
        b.remove().unwrap();
        assert_eq!(b.state(), BoxState::Missing);
    }
}
