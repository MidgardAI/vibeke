//! Server side of execution isolation (13): per-task sandbox contexts ("boxes"), wrapping pane
//! spawns through a [`vk_sandbox::Runner`], the per-pane broker socket (§4.1), egress
//! Interactions from the proxy (§7), isolated agent launches (`--yolo` / `--isolate`), restore
//! after a server restart and `pane.can_see_paths` for sandboxed panes (06 A11).
//!
//! Holders keep owning the PTY on the host; for the `sandbox` level the holder's child is
//! `sandbox-exec -f <profile> $SHELL -l` (macOS) or the bubblewrap helper chain (Linux).

use crate::api::{Ctx, R, err, internal, invalid, s};
use crate::core::{Tx, ulid};
use crate::{Server, paths};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use vk_proto::model::*;
use vk_proto::rpc::{ErrorKind, Request, Response};
use vk_sandbox::config::{IsolationConfig, expand};
use vk_sandbox::container::{ContainerRunner, Provider};
use vk_sandbox::creds::{self, HarnessAuth, Projection, ProjectionInput};
use vk_sandbox::net::{EgressPolicy, NetworkProfile};
use vk_sandbox::proxy::{AskDecision, Asker, BoxFut, EgressEvent, EgressProxy, ProxyConfig};
use vk_sandbox::runner::{Runner, SandboxRunner, SandboxSetup, SpawnRequest};

pub const METHODS: &[(&str, bool)] = &[
    ("sandbox.status", false),
    ("sandbox.list", false),
    ("sandbox.allow", true),
];

/// Methods a contained process may call through its broker (13 §4.1): its own adapter
/// signals/gates, self-report, and read-only views of itself. Never `interaction.answer`,
/// `pane.send_*`, layout, tasks, plugins or elevation.
pub const BROKER_METHODS: &[&str] = &[
    "client.hello",
    "adapter.signal",
    "adapter.gate",
    "adapter.delivery_ack",
    "agent.report",
    "agent.get",
    "pane.current",
    "preview.declare",
];

/// How long an unanswered egress Interaction stays open (the proxy itself holds a connection
/// for at most `ask_timeout`; later attempts join the same Interaction).
const EGRESS_INTERACTION_TTL: Duration = Duration::from_secs(10 * 60);
/// After the user denies a destination, further attempts are refused without asking again.
const EGRESS_DENY_MEMORY: Duration = Duration::from_secs(10 * 60);

/// What a task (or an ad-hoc isolated agent launch) asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IsoRequest {
    pub level: IsolationLevel,
    pub network: NetworkProfile,
    pub yolo: bool,
    pub harnesses: Vec<String>,
    pub local_ports: Vec<u16>,
    pub image: Option<String>,
    /// Port the proxy listened on (restored after a server restart when possible).
    #[serde(default)]
    pub proxy_port: Option<u16>,
}

impl IsoRequest {
    /// From `task.create` / `agent.start` params: `isolate`, `yolo`, `network`, `image`.
    /// `None` = host without yolo (nothing to do).
    pub fn from_params(
        p: &Value,
        cfg: &IsolationConfig,
    ) -> Result<IsoRequest, vk_proto::rpc::RpcError> {
        let yolo = p.get("yolo").and_then(Value::as_bool).unwrap_or(false);
        let level = match s(p, "isolate") {
            Some(l) => IsolationLevel::parse(l).ok_or_else(|| {
                invalid(format!(
                    "unknown isolation level {l} (host|sandbox|container|vm)"
                ))
            })?,
            None if yolo => cfg.yolo_level(),
            None => IsolationLevel::Host,
        };
        let network = match s(p, "network") {
            Some(n) => NetworkProfile::parse(n).ok_or_else(|| {
                invalid(format!(
                    "unknown network profile {n} (none|harness-apis|package-registries|dev|open)"
                ))
            })?,
            None => cfg.network_profile(),
        };
        Ok(IsoRequest {
            level,
            network,
            yolo,
            harnesses: vec![],
            local_ports: cfg.sandbox.local_ports.clone(),
            image: s(p, "image")
                .map(str::to_string)
                .or(cfg.container.image.clone()),
            proxy_port: None,
        })
    }
}

pub enum BoxRunner {
    Sandbox(Box<SandboxRunner>),
    Container(ContainerRunner),
}

impl BoxRunner {
    fn runner(&self) -> &dyn Runner {
        match self {
            BoxRunner::Sandbox(r) => r.as_ref(),
            BoxRunner::Container(r) => r,
        }
    }
}

/// One contained context: a task's panes, or one isolated agent launched into a host pane.
pub struct TaskBox {
    pub key: String,
    pub task: Option<String>,
    pub checkout: PathBuf,
    pub runner: BoxRunner,
    pub proxy: Option<EgressProxy>,
    pub isolation: Isolation,
    pub request: IsoRequest,
    pub projection_names: Vec<String>,
    pub created: Instant,
}

#[derive(Default)]
struct Inner {
    boxes: HashMap<String, Arc<TaskBox>>,
    by_checkout: Vec<(PathBuf, String)>,
    brokers: HashMap<String, JoinHandle<()>>,
    /// Pending egress decisions per (box, host): later attempts join the same Interaction.
    waits: HashMap<(String, String), watch::Receiver<Option<AskDecision>>>,
    denied: HashMap<(String, String), Instant>,
    /// Test override for the host home directory.
    home: Option<PathBuf>,
    ticking: bool,
}

#[derive(Default)]
pub struct State {
    inner: Mutex<Inner>,
}

impl State {
    /// Use `home` instead of `$HOME` for allowlists and credential projection (tests).
    pub fn set_home(&self, home: PathBuf) {
        self.inner.lock().unwrap().home = Some(home);
    }
    pub fn get(&self, key: &str) -> Option<Arc<TaskBox>> {
        self.inner.lock().unwrap().boxes.get(key).cloned()
    }
    fn home(&self) -> PathBuf {
        self.inner
            .lock()
            .unwrap()
            .home
            .clone()
            .unwrap_or_else(paths::home)
    }
}

pub fn load_cfg() -> IsolationConfig {
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    let (c, e) = IsolationConfig::from_toml(cfg.extra.get("isolation"));
    if let Some(e) = e {
        tracing::warn!(error = %e, "invalid [isolation] config; using defaults");
    }
    c
}

/// Vibeke-private paths a contained process must never see (13 §4.1), plus the user's
/// `$TMPDIR` (other tools' scratch files and, on macOS, Vibeke's runtime dir).
fn hidden_paths() -> Vec<PathBuf> {
    let mut v = vec![paths::runtime_root(), paths::state_root()];
    if let Some(d) = vk_config::config_path().parent() {
        v.push(d.to_path_buf());
    }
    if let Some(t) = std::env::var_os("TMPDIR") {
        v.push(PathBuf::from(t));
    }
    v
}

fn credentials_dir() -> PathBuf {
    paths::state_root().join("credentials")
}

fn sbx_root(key: &str) -> PathBuf {
    paths::state_root()
        .join("sbx")
        .join(vk_sandbox::runner::short_id(key))
}

/// Approval-bypass flags per harness (manifest `[yolo] flags`, 04/13 §3).
pub fn yolo_args(harness: &str) -> Vec<String> {
    match harness {
        "claude" => vec!["--dangerously-skip-permissions".into()],
        "codex" => vec!["--dangerously-bypass-approvals-and-sandbox".into()],
        // Unverified against omp releases (spec 13 status); detection accepts both spellings.
        "omp" => vec!["--approval".into(), "off".into()],
        _ => vec![], // pi has no approval system
    }
}

fn project_all(
    server: &Server,
    harnesses: &[String],
    private: &Path,
    trust: Option<&Path>,
) -> Result<Projection, std::io::Error> {
    let home = server.sandbox.home();
    let creds_dir = credentials_dir();
    let mut parts = Vec::new();
    for h in harnesses {
        if let Some(a) = HarnessAuth::from_id(h) {
            parts.push(creds::project(
                a,
                &ProjectionInput {
                    home: &home,
                    host_env: &server.opts.env,
                    vibeke_credentials: &creds_dir,
                    private_dir: private,
                    trust_checkout: trust,
                    claude_dir: None,
                    codex_dir: None,
                    pi_agent_dir: None,
                },
            )?);
        }
    }
    Ok(creds::merge(parts))
}

fn emit(server: &Server, kind: &str, subject: Value, data: Value) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(kind, subject, data);
    let _ = server.commit(&mut c, tx);
}

struct ServerAsker {
    server: Weak<Server>,
    key: String,
}

impl Asker for ServerAsker {
    fn ask(&self, host: String, port: u16) -> BoxFut<AskDecision> {
        let server = self.server.upgrade();
        let key = self.key.clone();
        Box::pin(async move {
            match server {
                Some(s) => egress_ask(&s, &key, host, port).await,
                None => AskDecision::Deny,
            }
        })
    }
}

/// Create (or prepare after a restart) a contained context and its egress proxy.
pub async fn prepare_box(
    server: &Arc<Server>,
    key: &str,
    task: Option<&str>,
    checkout: &Path,
    mut req: IsoRequest,
) -> Result<Arc<TaskBox>, vk_proto::rpc::RpcError> {
    if let Some(b) = server.sandbox.get(key) {
        return Ok(b);
    }
    let cfg = load_cfg();
    let home = server.sandbox.home();
    let root = sbx_root(key);
    std::fs::create_dir_all(&root).map_err(internal)?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
    }
    let checkout = checkout
        .canonicalize()
        .unwrap_or_else(|_| checkout.to_path_buf());
    let mut proxy = None;
    if req.level != IsolationLevel::Host && req.network.uses_proxy() {
        let mut pol = EgressPolicy::new(req.network);
        pol.extra_allow
            .extend(cfg.sandbox.allow_domains.iter().cloned());
        pol.deny.extend(cfg.sandbox.deny_domains.iter().cloned());
        pol.local_ports
            .extend(cfg.sandbox.local_ports.iter().copied());
        pol.allow_private = cfg.sandbox.allow_private;
        let mut pc = ProxyConfig::new(pol);
        pc.asker = Some(Arc::new(ServerAsker {
            server: Arc::downgrade(server),
            key: key.to_string(),
        }));
        let weak = Arc::downgrade(server);
        let subject = json!({"task": task, "sandbox": key});
        let last_denied: Arc<Mutex<HashMap<String, Instant>>> = Arc::default();
        pc.observer = Some(Arc::new(move |e: EgressEvent| {
            let Some(srv) = weak.upgrade() else { return };
            match e {
                EgressEvent::Allowed { host, port, rule } => {
                    // Only non-profile rules are worth an event (task approvals, open profile).
                    if rule.starts_with("task:")
                        || rule.contains("open")
                        || rule.starts_with("approved")
                    {
                        emit(
                            &srv,
                            "sandbox.egress_allowed",
                            subject.clone(),
                            json!({"host": host, "port": port, "rule": rule}),
                        );
                    }
                }
                EgressEvent::Denied { host, port, reason } => {
                    // Rate limit: one event per host per 10 s.
                    let mut m = last_denied.lock().unwrap();
                    let now = Instant::now();
                    if m.get(&host)
                        .is_some_and(|t| now.duration_since(*t) < Duration::from_secs(10))
                    {
                        return;
                    }
                    m.insert(host.clone(), now);
                    drop(m);
                    emit(
                        &srv,
                        "sandbox.egress_denied",
                        subject.clone(),
                        json!({"host": host, "port": port, "reason": reason}),
                    );
                }
            }
        }));
        let unix = cfg!(target_os = "linux").then(|| root.join("egress.sock"));
        let p =
            match EgressProxy::start(pc.clone(), req.proxy_port.unwrap_or(0), unix.clone()).await {
                Ok(p) => p,
                Err(_) if req.proxy_port.is_some() => {
                    EgressProxy::start(pc, 0, unix).await.map_err(internal)?
                }
                Err(e) => return Err(internal(e)),
            };
        req.proxy_port = Some(p.port);
        proxy = Some(p);
    }
    let trust = req.yolo.then_some(checkout.as_path());
    let projection =
        project_all(server, &req.harnesses, &root.join("shared"), trust).map_err(internal)?;
    let projection_names = projection.summary();
    let (runner, provider) = match req.level {
        IsolationLevel::Sandbox => {
            let mut extra_read = vec![paths::Paths::inbox(), paths::Paths::shims()];
            extra_read.extend(cfg.sandbox.read.iter().map(|r| expand(&home, r)));
            let setup = SandboxSetup {
                home: home.clone(),
                checkout: checkout.clone(),
                git: vk_sandbox::GitLayout::detect(&checkout),
                root: root.clone(),
                network: req.network,
                proxy_port: req.proxy_port,
                local_ports: req.local_ports.clone(),
                extra_read,
                extra_write: cfg.sandbox.write.iter().map(|w| expand(&home, w)).collect(),
                hidden: hidden_paths(),
                home_read: None,
                projection,
                vibeke_bin: Some(server.opts.bin.clone()),
                egress_socket: proxy.as_ref().and_then(|p| p.unix_socket.clone()),
                broker: cfg.sandbox.broker,
            };
            let r = SandboxRunner { setup };
            r.check()
                .map_err(|e| err(ErrorKind::Unsupported, e.to_string()))?;
            let prov = r.provider();
            (BoxRunner::Sandbox(Box::new(r)), prov)
        }
        IsolationLevel::Container => {
            let provider = vk_sandbox::container::detect().ok_or_else(|| {
                err(
                    ErrorKind::Unsupported,
                    "no container runtime found (Apple container, OrbStack, Docker, Podman)",
                )
            })?;
            let image = req.image.clone().ok_or_else(|| {
                invalid(
                    "container isolation needs an image (--image or [isolation.container] image)",
                )
            })?;
            let r = ContainerRunner {
                provider: provider.clone(),
                image,
                network: req.network,
                checkout: checkout.clone(),
                mounts: vec![vk_sandbox::runner::Mount {
                    host: paths::Paths::inbox(),
                    target: "/vibeke/inbox".into(),
                    read_only: true,
                }],
                pass_env: projection
                    .env
                    .iter()
                    .filter(|(k, _)| !k.ends_with("_DIR") && k != "CODEX_HOME")
                    .map(|(k, _)| k.clone())
                    .collect(),
                cpus: cfg.container.cpus.clone(),
                memory: cfg.container.memory.clone(),
            };
            // Validate the network profile up front (proxy profiles are refused for now).
            r.run_argv("vk-check", &checkout, &[])
                .map_err(|e| err(ErrorKind::Unsupported, e.to_string()))?;
            let name = match provider {
                Provider::AppleContainer(_) => "apple-container",
                Provider::OrbStack(_) => "orbstack",
                Provider::Docker(_) => "docker",
                Provider::Podman(_) => "podman",
            };
            (BoxRunner::Container(r), name)
        }
        IsolationLevel::Vm => {
            return Err(err(
                ErrorKind::Unsupported,
                vk_sandbox::VmRunner.check().unwrap_err().to_string(),
            ));
        }
        IsolationLevel::Host => {
            return Err(invalid("host needs no sandbox context"));
        }
    };
    let isolation = Isolation {
        level: req.level,
        provider: provider.to_string(),
        network: req.network.as_str().to_string(),
        yolo: req.yolo,
        scope: if task.is_some() {
            "pane".into()
        } else {
            "run".into()
        },
        visible_roots: vec![],
    };
    let b = Arc::new(TaskBox {
        key: key.to_string(),
        task: task.map(str::to_string),
        checkout: checkout.clone(),
        runner,
        proxy,
        isolation,
        request: req.clone(),
        projection_names: projection_names.clone(),
        created: Instant::now(),
    });
    {
        let mut i = server.sandbox.inner.lock().unwrap();
        i.boxes.insert(key.to_string(), b.clone());
        if task.is_some() {
            i.by_checkout.push((checkout.clone(), key.to_string()));
        }
    }
    // Persist what is needed to restore the context after a server restart (no secrets).
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.kv(
            "sandbox",
            key,
            Some(
                serde_json::to_string(&json!({"task": task, "checkout": checkout, "request": req}))
                    .unwrap_or_default(),
            ),
        );
        tx.event(
            "sandbox.created",
            json!({"task": task, "sandbox": key}),
            json!({"level": req.level.as_str(), "provider": provider, "network": req.network.as_str(), "yolo": req.yolo, "proxy_port": req.proxy_port, "credentials": projection_names}),
        );
        let _ = server.commit(&mut c, tx);
    }
    ensure_tick(server);
    Ok(b)
}

/// The box whose panes include a pane spawned in `cwd` (for workspace `ws_task`).
fn box_for_spawn(server: &Server, ws_task: Option<&str>, cwd: &str) -> Option<Arc<TaskBox>> {
    let i = server.sandbox.inner.lock().unwrap();
    if let Some(t) = ws_task
        && let Some(b) = i.boxes.get(t)
    {
        return Some(b.clone());
    }
    let cwd = Path::new(cwd);
    i.by_checkout
        .iter()
        .find(|(co, _)| cwd.starts_with(co) || cwd.canonicalize().is_ok_and(|c| c.starts_with(co)))
        .and_then(|(_, k)| i.boxes.get(k).cloned())
}

/// argv, env and the pane's isolation record.
pub type WrappedSpawn = (Vec<String>, Vec<(String, String)>, Isolation);

/// Wrap a pane spawn (called by the server's holder spawn path; must not lock `core`).
/// Host panes pass through unchanged.
pub fn wrap_spawn(
    server: &Arc<Server>,
    pane_id: &str,
    cwd: &str,
    argv: &[String],
    env: Vec<(String, String)>,
    ws_task: Option<&str>,
) -> anyhow::Result<WrappedSpawn> {
    let Some(b) = box_for_spawn(server, ws_task, cwd) else {
        return Ok((argv.to_vec(), env, Isolation::default()));
    };
    if b.task.is_none() {
        return Ok((argv.to_vec(), env, Isolation::default()));
    }
    let prepared = b.runner.runner().prepare(SpawnRequest {
        pane_id: pane_id.to_string(),
        argv: argv.to_vec(),
        cwd: PathBuf::from(cwd),
        env,
    })?;
    if let Some(sock) = &prepared.broker_socket {
        start_broker(server, pane_id, sock);
    }
    let mut iso = b.isolation.clone();
    iso.scope = "pane".into();
    iso.visible_roots = prepared.visible_roots.clone();
    Ok((prepared.argv, prepared.env, iso))
}

// ---- broker (13 §4.1) -------------------------------------------------------------------------

fn start_broker(server: &Arc<Server>, pane_id: &str, path: &Path) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let _ = std::fs::remove_file(path);
    let l = match std::os::unix::net::UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "sandbox broker bind failed; hooks inside the box will not report");
            return;
        }
    };
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    let _ = l.set_nonblocking(true);
    let srv = server.clone();
    let pane = pane_id.to_string();
    let _guard = handle.enter();
    let Ok(l) = tokio::net::UnixListener::from_std(l) else {
        return;
    };
    let task = handle.spawn(async move {
        loop {
            let Ok((stream, _)) = l.accept().await else {
                continue;
            };
            let (srv, pane) = (srv.clone(), pane.clone());
            tokio::spawn(async move {
                let _ = broker_connection(srv, stream, pane).await;
            });
        }
    });
    if let Some(old) = server
        .sandbox
        .inner
        .lock()
        .unwrap()
        .brokers
        .insert(pane_id.to_string(), task)
    {
        old.abort();
    }
}

/// One broker connection: pane scope is fixed by the socket, tokens can't change it, and only
/// [`BROKER_METHODS`] are served.
pub async fn broker_connection<S>(
    server: Arc<Server>,
    stream: S,
    pane: String,
) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (rd, mut wr) = tokio::io::split(stream);
    let mut rd = BufReader::new(rd);
    let ctx = Ctx {
        client_id: format!("broker-{}", vk_sandbox::runner::short_id(&pane)),
        kind: "agent".into(),
        pane_scope: Some(pane.clone()),
        remote: false,
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let mut line = String::new();
    loop {
        tokio::select! {
            n = rd.read_line(&mut line) => {
                if n? == 0 { break }
                let l = std::mem::take(&mut line);
                let l = l.trim_end_matches(['\n', '\r']).to_string();
                if l.is_empty() { continue }
                let req: Option<Request> = serde_json::from_str(&l).ok();
                let method = req.as_ref().map(|r| r.method.clone()).unwrap_or_default();
                if !BROKER_METHODS.contains(&method.as_str()) {
                    let id = req.and_then(|r| r.id).unwrap_or(Value::Null);
                    let r = Response::err(id, err(ErrorKind::PermissionDenied, format!("{method} is not available inside a sandbox (broker, 13 §4.1)")).details(json!({"scope": "broker"})));
                    let _ = tx.send(serde_json::to_string(&r)?);
                    continue;
                }
                let (srv, c, t) = (server.clone(), ctx.clone(), tx.clone());
                tokio::spawn(async move { let _ = t.send(crate::api::handle_line(&srv, &c, &l).await); });
            }
            Some(out) = rx.recv() => {
                wr.write_all(out.as_bytes()).await?;
                wr.write_all(b"\n").await?;
                wr.flush().await?;
            }
        }
    }
    Ok(())
}

// ---- egress Interactions (13 §7) --------------------------------------------------------------

fn decision_of(it: &Interaction) -> AskDecision {
    match it.answer.as_ref().and_then(|a| a.decision) {
        Some(Decision::Allow) => AskDecision::AllowOnce,
        Some(Decision::AllowAlways) => AskDecision::AllowTask,
        _ => AskDecision::Deny,
    }
}

/// Ask the user about `host:port` for box `key`. Concurrent attempts share one Interaction.
pub async fn egress_ask(server: &Arc<Server>, key: &str, host: String, port: u16) -> AskDecision {
    let wkey = (key.to_string(), host.clone());
    let existing = {
        let mut i = server.sandbox.inner.lock().unwrap();
        if let Some(t) = i.denied.get(&wkey) {
            if t.elapsed() < EGRESS_DENY_MEMORY {
                return AskDecision::Deny;
            }
            i.denied.remove(&wkey);
        }
        i.waits.get(&wkey).cloned()
    };
    let mut rx = match existing {
        Some(rx) => rx,
        None => match open_egress_interaction(server, key, &host, port) {
            Some(rx) => rx,
            None => return AskDecision::Deny,
        },
    };
    loop {
        if let Some(d) = *rx.borrow() {
            return d;
        }
        if rx.changed().await.is_err() {
            return rx.borrow().unwrap_or(AskDecision::Deny);
        }
    }
}

fn open_egress_interaction(
    server: &Arc<Server>,
    key: &str,
    host: &str,
    port: u16,
) -> Option<watch::Receiver<Option<AskDecision>>> {
    let b = server.sandbox.get(key)?;
    // Attach to the task's agent pane if there is one (it shows on that agent's row).
    let (pane, run, task_handle) = server.with_core(|c| {
        let task = b.task.as_ref().and_then(|t| c.task(t).cloned());
        let ws = task.as_ref().and_then(|t| t.workspace.clone());
        let panes: Vec<&Pane> = c
            .model
            .panes
            .iter()
            .filter(|p| match &ws {
                Some(w) => &p.workspace == w,
                None => b.key == format!("pane:{}", p.id),
            })
            .collect();
        let with_run = panes
            .iter()
            .find_map(|p| c.run_for_pane(&p.id).map(|r| (p.id.clone(), r.id.clone())));
        let (pane, run) = with_run
            .or_else(|| panes.first().map(|p| (p.id.clone(), String::new())))
            .unwrap_or_default();
        (pane, run, task.map(|t| t.handle).unwrap_or_default())
    });
    if pane.is_empty() {
        return None;
    }
    let (tx, rx) = watch::channel(None);
    let id = ulid();
    let profile = b.isolation.network.clone();
    let it = {
        let mut c = server.core.lock().unwrap();
        let it = Interaction {
            id: id.clone(),
            handle: c.next_interaction_handle(),
            run: run.clone(),
            pane: pane.clone(),
            kind: InteractionKind::Approval,
            status: InteractionStatus::Open,
            title: format!("Allow network access to {host}:{port}?"),
            body_md: Some(format!(
                "A sandboxed process in {} wants to reach `{host}:{port}`, which is not on the `{profile}` network profile.\n\n**allow** = this connection only · **allow always** = for this task · **deny**",
                if task_handle.is_empty() {
                    "this pane".to_string()
                } else {
                    format!("task {task_handle}")
                }
            )),
            action: Some(ActionInfo {
                tool: "egress".into(),
                summary: format!("connect {host}:{port}"),
                command: None,
                paths: vec![],
                diff: None,
                risk: Risk::Medium,
                risk_reasons: vec!["network egress outside the sandbox allowlist".into()],
            }),
            questions: vec![],
            plan_md: None,
            answer_channel: AnswerChannel::Native,
            native_ref: Some(format!("egress:{key}:{host}")),
            source: StateSource::Structured,
            confidence: 1.0,
            answerable: true,
            gate: true,
            decision_rev: 0,
            delivery: DeliveryState::None,
            delivery_error: None,
            answer: None,
            answered_by: None,
            opened_at_ms: vk_store::now_ms(),
            answered_at_ms: None,
        };
        let mut t = Tx::new();
        t.counters = true;
        t.event(
            "interaction.opened",
            json!({"interaction": id, "pane": pane, "run": run}),
            json!({"kind": "approval", "source": "sandbox", "egress": {"host": host, "port": port}}),
        );
        t.interaction(it.clone());
        if server.commit(&mut c, t).is_err() {
            return None;
        }
        it
    };
    server
        .sandbox
        .inner
        .lock()
        .unwrap()
        .waits
        .insert((key.to_string(), host.to_string()), rx.clone());
    server.notify(
        "interaction",
        Some(&pane),
        "sandbox wants network access",
        &it.title,
        "normal",
    );
    let gate = crate::agents::hold_external_gate(server, &id, &pane);
    let srv = server.clone();
    let (key, host) = (key.to_string(), host.to_string());
    tokio::spawn(async move {
        let answered = tokio::time::timeout(EGRESS_INTERACTION_TTL, gate).await;
        let decision = match answered {
            Ok(Ok(true)) => srv
                .with_core(|c| c.interaction(&id).cloned())
                .map(|it| decision_of(&it))
                .unwrap_or(AskDecision::Deny),
            _ => {
                crate::agents::close_interaction(
                    &srv,
                    &id,
                    InteractionStatus::Expired,
                    "egress decision timed out",
                );
                AskDecision::Deny
            }
        };
        match decision {
            AskDecision::AllowTask => {
                if let Some(b) = srv.sandbox.get(&key)
                    && let Some(p) = &b.proxy
                {
                    p.allow_for_task(&host);
                }
            }
            AskDecision::Deny => {
                srv.sandbox
                    .inner
                    .lock()
                    .unwrap()
                    .denied
                    .insert((key.clone(), host.clone()), Instant::now());
            }
            AskDecision::AllowOnce => {}
        }
        if matches!(answered, Ok(Ok(true))) {
            // The decision is applied by the proxy: delivery confirmed.
            crate::agents::close_interaction(
                &srv,
                &id,
                InteractionStatus::Answered,
                "applied by egress proxy",
            );
        }
        srv.sandbox
            .inner
            .lock()
            .unwrap()
            .waits
            .remove(&(key.clone(), host.clone()));
        let _ = tx.send(Some(decision));
    });
    Some(rx)
}

// ---- isolated agent launches (13 §3) ----------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct LaunchOpts {
    pub yolo: bool,
    pub isolate: Option<IsolationLevel>,
    pub network: Option<NetworkProfile>,
}

impl LaunchOpts {
    pub fn from_params(p: &Value) -> Result<LaunchOpts, vk_proto::rpc::RpcError> {
        Ok(LaunchOpts {
            yolo: p.get("yolo").and_then(Value::as_bool).unwrap_or(false),
            isolate: match s(p, "isolate") {
                Some(l) => Some(
                    IsolationLevel::parse(l)
                        .ok_or_else(|| invalid(format!("unknown isolation level {l}")))?,
                ),
                None => None,
            },
            network: match s(p, "network") {
                Some(n) => Some(
                    NetworkProfile::parse(n)
                        .ok_or_else(|| invalid(format!("unknown network profile {n}")))?,
                ),
                None => None,
            },
        })
    }
}

/// The command line typed into the pane and the effective argv (for yolo detection/events).
pub struct AgentLaunch {
    pub argv: Vec<String>,
    pub line: String,
}

fn shell_line(argv: &[String]) -> String {
    crate::agents::harness::shell_join(argv)
}

/// Apply `--yolo` / `--isolate` to an agent launch in `pane_id`.
pub async fn prepare_agent(
    server: &Arc<Server>,
    pane_id: &str,
    harness: &str,
    mut argv: Vec<String>,
    opts: &LaunchOpts,
) -> Result<AgentLaunch, vk_proto::rpc::RpcError> {
    let pane = server
        .with_core(|c| c.pane(pane_id).cloned())
        .ok_or_else(|| crate::api::not_found("pane", pane_id))?;
    let cfg = load_cfg();
    let contained = pane.isolation.is_contained() && pane.isolation.scope == "pane";
    if contained && opts.isolate == Some(IsolationLevel::Host) {
        return Err(invalid(
            "this pane is sandboxed; an agent in it cannot run on the host",
        ));
    }
    let level = if contained {
        pane.isolation.level
    } else {
        opts.isolate.unwrap_or(if opts.yolo {
            cfg.yolo_level()
        } else {
            IsolationLevel::Host
        })
    };
    if opts.yolo {
        let flags = yolo_args(harness);
        argv.splice(1..1, flags);
    }
    // Codex's own Seatbelt sandbox cannot nest inside ours: Vibeke's sandbox replaces it, while
    // approvals stay as configured.
    if level == IsolationLevel::Sandbox && harness == "codex" && !opts.yolo {
        argv.splice(
            1..1,
            ["--sandbox".to_string(), "danger-full-access".to_string()],
        );
    }
    let bin = server.opts.bin.to_string_lossy().into_owned();
    if level == IsolationLevel::Host {
        return Ok(AgentLaunch {
            line: shell_line(&argv),
            argv,
        });
    }
    if contained {
        // Already inside the task's sandbox: project this harness's credentials into the
        // pane's private dir and launch through the env-only helper.
        let key = server
            .sandbox
            .inner
            .lock()
            .unwrap()
            .by_checkout
            .iter()
            .find(|(co, _)| {
                pane.cwd
                    .as_deref()
                    .is_some_and(|c| Path::new(c).starts_with(co))
            })
            .map(|(_, k)| k.clone());
        let task_key = server
            .with_core(|c| c.ws(&pane.workspace).and_then(|w| w.task.clone()))
            .or(key);
        let b = task_key.and_then(|k| server.sandbox.get(&k));
        let Some(BoxRunner::Sandbox(r)) = b.as_ref().map(|b| &b.runner) else {
            return Ok(AgentLaunch {
                line: shell_line(&argv),
                argv,
            });
        };
        let private = r.pane_dir(pane_id);
        let trust = opts.yolo.then(|| r.setup.checkout.clone());
        let pr = project_all(server, &[harness.to_string()], &private, trust.as_deref())
            .map_err(internal)?;
        if pr.env.is_empty() {
            return Ok(AgentLaunch {
                line: shell_line(&argv),
                argv,
            });
        }
        let spec_path = private.join(format!("launch-{}.json", &ulid()[20..]));
        vk_sandbox::exec::write_spec(
            &spec_path,
            &vk_sandbox::exec::ExecSpec {
                argv: argv.clone(),
                env_clear: false,
                env: pr.env.clone(),
                profile: None,
                cwd: None,
            },
        )
        .map_err(internal)?;
        let line = shell_line(&[
            bin,
            "sandbox".into(),
            "exec".into(),
            "--spec".into(),
            spec_path.to_string_lossy().into_owned(),
        ]);
        return Ok(AgentLaunch { argv, line });
    }
    // Host pane, isolated agent: an ad-hoc box scoped to this run.
    let cwd = server
        .pane_cwd(pane_id)
        .or(pane.cwd.clone())
        .unwrap_or_else(|| paths::home().to_string_lossy().into_owned());
    let checkout = vk_tasks::repo_root(Path::new(&cwd))
        .map(|r| r.worktree_root)
        .unwrap_or_else(|| PathBuf::from(&cwd));
    let req = IsoRequest {
        level,
        network: opts.network.unwrap_or(cfg.network_profile()),
        yolo: opts.yolo,
        harnesses: vec![harness.to_string()],
        local_ports: cfg.sandbox.local_ports.clone(),
        image: cfg.container.image.clone(),
        proxy_port: None,
    };
    let key = format!("pane:{pane_id}");
    // A fresh launch replaces an earlier ad-hoc context for this pane.
    if let Some(old) = server.sandbox.inner.lock().unwrap().boxes.remove(&key) {
        drop(old);
    }
    let b = prepare_box(server, &key, None, &checkout, req).await?;
    let (handle, tabh, wsh) = server.with_core(|c| {
        (
            pane.handle.clone(),
            c.tab(&pane.tab)
                .map(|t| t.handle.clone())
                .unwrap_or_default(),
            c.ws(&pane.workspace)
                .map(|w| w.handle.clone())
                .unwrap_or_default(),
        )
    });
    let host_env = server.pane_env_for(pane_id, &handle, &tabh, &wsh);
    let prepared = b
        .runner
        .runner()
        .prepare(SpawnRequest {
            pane_id: pane_id.to_string(),
            argv: argv.clone(),
            cwd: PathBuf::from(&cwd),
            env: host_env,
        })
        .map_err(|e| err(ErrorKind::Unsupported, e.to_string()))?;
    if let Some(sock) = &prepared.broker_socket {
        start_broker(server, pane_id, sock);
    }
    let private = prepared
        .profile
        .as_ref()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| sbx_root(&key));
    let spec_path = private.join(format!("launch-{}.json", &ulid()[20..]));
    vk_sandbox::exec::write_spec(
        &spec_path,
        &vk_sandbox::exec::ExecSpec {
            argv: prepared.argv.clone(),
            env_clear: true,
            env: prepared.env.clone(),
            profile: None,
            cwd: Some(prepared.cwd.clone()),
        },
    )
    .map_err(internal)?;
    let mut iso = b.isolation.clone();
    iso.scope = "run".into();
    iso.visible_roots = prepared.visible_roots.clone();
    set_pane_isolation(server, pane_id, iso);
    let line = shell_line(&[
        bin,
        "sandbox".into(),
        "exec".into(),
        "--spec".into(),
        spec_path.to_string_lossy().into_owned(),
    ]);
    Ok(AgentLaunch { argv, line })
}

fn set_pane_isolation(server: &Server, pane_id: &str, iso: Isolation) {
    let mut c = server.core.lock().unwrap();
    if let Some(mut p) = c.pane(pane_id).cloned() {
        if p.isolation == iso {
            return;
        }
        p.isolation = iso.clone();
        let mut tx = Tx::new();
        tx.event(
            "pane.isolation_changed",
            json!({"pane": p.id}),
            json!({"level": iso.level.as_str(), "scope": iso.scope, "network": iso.network}),
        );
        tx.pane(p);
        let _ = server.commit(&mut c, tx);
    }
}

// ---- lifecycle --------------------------------------------------------------------------------

/// Tear down a task's context (task finish/archive): proxy, brokers, private dirs.
pub fn teardown(server: &Server, key: &str) {
    let removed = {
        let mut i = server.sandbox.inner.lock().unwrap();
        i.by_checkout.retain(|(_, k)| k != key);
        i.boxes.remove(key)
    };
    if let Some(b) = removed {
        emit(
            server,
            "sandbox.destroyed",
            json!({"task": b.task, "sandbox": key}),
            json!({}),
        );
        let root = sbx_root(key);
        drop(b);
        let _ = std::fs::remove_dir_all(root);
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.kv("sandbox", key, None);
        let _ = server.commit(&mut c, tx);
    }
}

fn ensure_tick(server: &Arc<Server>) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    {
        let mut i = server.sandbox.inner.lock().unwrap();
        if i.ticking {
            return;
        }
        i.ticking = true;
    }
    let weak = Arc::downgrade(server);
    handle.spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(2));
        loop {
            t.tick().await;
            let Some(srv) = weak.upgrade() else { return };
            tick(&srv);
        }
    });
}

/// Drop brokers of closed panes and end run-scoped contexts whose agent is gone.
fn tick(server: &Arc<Server>) {
    let (live, runs): (Vec<String>, Vec<String>) = server.with_core(|c| {
        (
            c.model.panes.iter().map(|p| p.id.clone()).collect(),
            c.model
                .runs
                .iter()
                .filter(|r| r.ended_at_ms.is_none())
                .map(|r| r.pane.clone())
                .collect(),
        )
    });
    let mut revert = Vec::new();
    {
        let mut i = server.sandbox.inner.lock().unwrap();
        i.brokers.retain(|pane, h| {
            let keep = live.contains(pane);
            if !keep {
                h.abort();
            }
            keep
        });
        let ended: Vec<String> = i
            .boxes
            .keys()
            .filter_map(|k| k.strip_prefix("pane:").map(str::to_string))
            .filter(|p| !runs.contains(p))
            .collect();
        for p in ended {
            revert.push(p);
        }
    }
    for p in revert {
        // Give a just-launched run a moment to register before tearing its context down.
        let fresh = server
            .sandbox
            .get(&format!("pane:{p}"))
            .is_some_and(|b| b.created.elapsed() < Duration::from_secs(15));
        if fresh {
            continue;
        }
        teardown(server, &format!("pane:{p}"));
        if let Some(h) = server.sandbox.inner.lock().unwrap().brokers.remove(&p) {
            h.abort();
        }
        set_pane_isolation(server, &p, Isolation::default());
    }
}

/// After a server restart: expire egress Interactions whose proxy is gone, rebuild task
/// contexts (proxy on the same port when free) and re-bind brokers for live sandboxed panes —
/// the contained processes themselves survived in their holders.
pub async fn restore(server: &Arc<Server>) {
    let stale: Vec<Interaction> = server.with_core(|c| {
        c.model
            .interactions
            .iter()
            .filter(|i| {
                i.status == InteractionStatus::Open
                    && i.native_ref
                        .as_deref()
                        .is_some_and(|r| r.starts_with("egress:"))
            })
            .cloned()
            .collect()
    });
    for mut it in stale {
        it.status = InteractionStatus::Expired;
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event(
            "interaction.expired",
            json!({"interaction": it.id, "pane": it.pane, "run": it.run}),
            json!({"reason": "server restarted; the proxy will ask again"}),
        );
        tx.interaction(it);
        let _ = server.commit(&mut c, tx);
    }
    let records: Vec<(String, Value)> = server.with_core(|c| {
        c.model
            .tasks
            .iter()
            .filter(|t| t.isolation.is_contained() && t.status == "active")
            .filter_map(|t| {
                c.store
                    .kv_get("sandbox", &t.id)
                    .ok()
                    .flatten()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .map(|v| (t.id.clone(), v))
            })
            .collect()
    });
    for (task, rec) in records {
        let Ok(req) = serde_json::from_value::<IsoRequest>(rec["request"].clone()) else {
            continue;
        };
        let checkout = PathBuf::from(rec["checkout"].as_str().unwrap_or_default());
        if let Err(e) = prepare_box(server, &task, Some(&task), &checkout, req).await {
            tracing::warn!(task, error = %e.message, "sandbox restore failed");
        }
    }
    let panes: Vec<(String, String, String)> = server.with_core(|c| {
        c.model
            .panes
            .iter()
            .filter(|p| p.isolation.level == IsolationLevel::Sandbox && p.isolation.scope == "pane")
            .map(|p| {
                (
                    p.id.clone(),
                    c.ws(&p.workspace)
                        .and_then(|w| w.task.clone())
                        .unwrap_or_default(),
                    p.cwd.clone().unwrap_or_default(),
                )
            })
            .collect()
    });
    for (pane, task, cwd) in panes {
        if let Some(b) = box_for_spawn(server, Some(&task), &cwd)
            && let BoxRunner::Sandbox(r) = &b.runner
            && r.setup.broker
        {
            start_broker(server, &pane, &r.pane_dir(&pane).join("b.sock"));
        }
    }
    ensure_tick(server);
}

// ---- queries ----------------------------------------------------------------------------------

/// Visibility of `path` from inside `pane` (06 A11.1). `None` when the pane is not contained.
pub fn can_see(server: &Server, pane: &str, path: &str) -> Option<bool> {
    let p = server.with_core(|c| c.pane(pane).cloned())?;
    if !p.isolation.is_contained() {
        return None;
    }
    if p.isolation.level != IsolationLevel::Sandbox {
        // Containers see only their mounts.
        return Some(
            p.isolation
                .visible_roots
                .iter()
                .any(|r| Path::new(path).starts_with(r)),
        );
    }
    let key = server
        .with_core(|c| c.ws(&p.workspace).and_then(|w| w.task.clone()))
        .unwrap_or_else(|| format!("pane:{pane}"));
    let b = server.sandbox.get(&key)?;
    let BoxRunner::Sandbox(r) = &b.runner else {
        return Some(false);
    };
    let private = r.pane_dir(pane);
    let policy = vk_sandbox::Policy::from_spec(&vk_sandbox::SandboxSpec {
        home: r.setup.home.clone(),
        checkout: r.setup.checkout.clone(),
        git: r.setup.git.clone(),
        private_dir: private,
        extra_read: {
            let mut v = r.setup.extra_read.clone();
            v.extend(r.setup.projection.read.iter().cloned());
            v
        },
        extra_write: vec![],
        read_only_files: vec![],
        hidden: r.setup.hidden.clone(),
        home_read: r.setup.home_read.clone(),
        unix_sockets: vec![],
        network: vk_sandbox::NetMode::None,
        allow_bind_localhost: false,
    });
    Some(policy.can_read(Path::new(path)))
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "sandbox.status" => {
            let levels: Vec<Value> = vk_sandbox::runner::availability()
                .into_iter()
                .map(|(l, r)| match r {
                    Ok(d) => json!({"level": l.as_str(), "available": true, "detail": d}),
                    Err(h) => json!({"level": l.as_str(), "available": false, "hint": h}),
                })
                .collect();
            Ok(json!({"levels": levels, "config": load_cfg()}))
        }
        "sandbox.list" => {
            let i = server.sandbox.inner.lock().unwrap();
            let list: Vec<Value> = i
                .boxes
                .values()
                .map(|b| {
                    json!({
                        "sandbox": b.key, "task": b.task, "checkout": b.checkout,
                        "level": b.isolation.level.as_str(), "provider": b.isolation.provider,
                        "network": b.isolation.network, "yolo": b.isolation.yolo,
                        "proxy_port": b.proxy.as_ref().map(|p| p.port),
                        "task_allow": b.proxy.as_ref().and_then(|p| p.policy.read().ok().map(|x| x.task_allow.clone())),
                        "credentials": b.projection_names,
                    })
                })
                .collect();
            Ok(json!({"sandboxes": list}))
        }
        "sandbox.allow" => {
            if ctx.pane_scope.is_some() {
                return Some(Err(err(
                    ErrorKind::PermissionDenied,
                    "sandbox.allow needs a user client",
                )));
            }
            let (Some(t), Some(host)) = (s(p, "task"), s(p, "host")) else {
                return Some(Err(invalid("task and host required")));
            };
            let key = server
                .with_core(|c| c.task(t).map(|x| x.id.clone()))
                .unwrap_or_else(|| t.to_string());
            match server
                .sandbox
                .get(&key)
                .and_then(|b| b.proxy.as_ref().map(|p| p.policy.clone()))
            {
                Some(pol) => {
                    if let Ok(mut w) = pol.write() {
                        w.task_allow.insert(host.to_ascii_lowercase());
                    }
                    emit(
                        server,
                        "sandbox.egress_allowed",
                        json!({"task": key}),
                        json!({"host": host, "rule": "user"}),
                    );
                    Ok(json!({"task": key, "allowed": host}))
                }
                None => Err(crate::api::not_found("sandbox", t)),
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod tests;
