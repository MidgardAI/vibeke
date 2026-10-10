//! Server side of the `cloud` level (spec 17 §4): a task's panes run in a hosted provider box
//! (Fly.io Sprites, E2B; the `fake` provider in tests). It mirrors the `container` level
//! (`sandbox_container.rs`): the holder stays on the host and every in-box operation is an exec
//! argv, here `vibeke cloud exec` (spec 17 §3.2), which speaks the provider protocol:
//!
//! - pane shells (`-i -t --session-file`, so a restarted holder reattaches);
//! - the bridge link, which carries the per-pane broker sockets (`vk_remote::boxlink`);
//! - the git services for the initial push and `task.sync` ([`BoxRemote`]).
//!
//! Short in-box commands the server itself runs (`uname`, the bootstrap script, the unsynced
//! report) go through [`exec_capture`], which talks to the provider directly. The provider
//! credential never enters the box, argv or events: `vibeke cloud exec` resolves it on the host.
//!
//! Box records (kv `cloud_box/<provider>/<id>`, [`BoxRecord`]) survive the task context; the
//! reconciler (`crate::cloud_reconcile`) keeps them in step with the providers.

use super::{BoxRunner, IsoRequest, TaskBox, control_dir, emit, sbx_root};
use crate::Server;
use crate::api::{err, internal, invalid};
use crate::core::{Core, Tx, ulid};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vk_cloud::{AuthMethod, CloudError, ExecReq, In, Out, Provider, Secret};
use vk_proto::rpc::{ErrorKind, RpcError};
use vk_sandbox::container::{
    BOX_BIN, BOX_BROKERS, BOX_CREDS, BOX_HOME, BOX_WORKSPACE, box_command, sh_quote,
};
use vk_sandbox::creds::Projection;
use vk_sandbox::runner::{PreparedSpawn, Runner, RunnerError, SpawnRequest, short_id};
use vk_tasks::sync::{self, BoxRemote, SyncOutcome};

/// `Isolation.network` of a cloud pane: the provider's own network (spec 17 §1 non-goals).
pub const NETWORK: &str = "provider";
/// kv scope of [`BoxRecord`]s, keyed `<provider>/<id>`.
pub const K_BOX: &str = "cloud_box";
/// kv scope of the host id (`cloud/host_id`) and signed-in account labels (`cloud/account:<p>`).
pub const K_CLOUD: &str = "cloud";
/// Per-pane file in the pane's control dir naming its detachable exec session.
pub const SESSION_FILE: &str = "cloud-session";
/// Shell for panes that asked for the host login shell.
pub const BOX_SHELL: &str = "bash";

// ---- records ------------------------------------------------------------------------------------

/// What is not on the host yet (spec 17 §6.3). `dirty` counts changed tracked files and stashes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unsynced {
    pub commits: u32,
    pub dirty: u32,
    pub untracked: u32,
    pub summary: String,
}

impl Unsynced {
    pub fn from_counts(commits: u32, dirty: u32, untracked: u32, stashes: u32) -> Unsynced {
        let plural =
            |n: u32, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let mut parts = Vec::new();
        if commits > 0 {
            parts.push(plural(commits, "commit", "commits"));
        }
        if dirty > 0 {
            parts.push(plural(dirty, "changed file", "changed files"));
        }
        if stashes > 0 {
            parts.push(plural(stashes, "stash", "stashes"));
        }
        if untracked > 0 {
            parts.push(plural(untracked, "untracked file", "untracked files"));
        }
        Unsynced {
            commits,
            dirty: dirty + stashes,
            untracked,
            summary: if parts.is_empty() {
                "clean".into()
            } else {
                format!("{} not on the host", parts.join(", "))
            },
        }
    }
    pub fn is_clean(&self) -> bool {
        self.commits == 0 && self.dirty == 0 && self.untracked == 0
    }
}

/// One box Vibeke knows about (kv `cloud_box/<provider>/<id>`). Never holds a credential.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BoxRecord {
    pub provider: String,
    pub id: String,
    pub name: String,
    /// Box key (task id) when this host created it; empty for boxes only seen in a listing.
    pub key: String,
    pub task: Option<String>,
    pub tags: Option<vk_cloud::naming::Tags>,
    /// Unix seconds.
    pub created_at: u64,
    pub last_activity_at: u64,
    /// [`vk_cloud::BoxState`] as a string, or `destroyed`.
    pub state: String,
    /// `attached | idle | orphaned | foreign | missing` (spec 17 §6.2).
    pub ownership: String,
    pub panes: Vec<String>,
    pub unsynced: Option<Unsynced>,
    pub workdir: String,
    /// Active provider sessions at the last reconcile.
    pub sessions: u32,
    pub url: Option<String>,
    /// The task's clone (host repo, host worktree, branch, base commit).
    pub repo: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub base: Option<String>,
}

impl BoxRecord {
    pub fn box_ref(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
    /// Created by this host for a task (as opposed to a box only seen in a listing).
    pub fn ours(&self) -> bool {
        !self.key.is_empty() && self.task.is_some()
    }
}

/// `<provider>/<id>` → (provider, id).
pub fn parse_box_ref(b: &str) -> Result<(String, String), RpcError> {
    match b.split_once('/') {
        Some((p, id)) if !p.is_empty() && !id.is_empty() && !id.contains('/') => {
            Ok((p.to_string(), id.to_string()))
        }
        _ => Err(invalid(format!("box must be <provider>/<id>, got {b:?}"))),
    }
}

pub fn load_record(server: &Server, box_ref: &str) -> Option<BoxRecord> {
    server
        .with_core(|c| c.store.kv_get(K_BOX, box_ref).ok().flatten())
        .and_then(|s| serde_json::from_str(&s).ok())
}

pub fn list_records(server: &Server) -> Vec<BoxRecord> {
    server
        .with_core(|c| c.store.kv_scope(K_BOX).unwrap_or_default())
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_str(&v).ok())
        .collect()
}

/// The record of the box that holds task `key`, if any.
pub fn record_for_key(server: &Server, key: &str) -> Option<BoxRecord> {
    list_records(server)
        .into_iter()
        .find(|r| r.key == key && r.state != "destroyed")
}

fn caps_of(provider: &str) -> vk_cloud::Caps {
    vk_cloud::provider(&cloud_cfg(), provider)
        .map(|p| p.caps())
        .unwrap_or_default()
}

/// Panes of `task`'s workspace that run in its cloud box (and the workspace).
pub fn cloud_panes(c: &Core, task: Option<&str>) -> (Option<String>, Vec<String>) {
    let Some(t) = task.and_then(|t| c.task(t)) else {
        return (None, vec![]);
    };
    let ws = t.workspace.clone();
    let panes = c
        .model
        .panes
        .iter()
        .filter(|p| {
            ws.as_deref() == Some(p.workspace.as_str())
                && p.isolation.level == vk_proto::model::IsolationLevel::Cloud
        })
        .map(|p| p.id.clone())
        .collect();
    (ws, panes)
}

/// `BoxView` (spec 17 §6.3) of a record, against the current model.
pub fn view_in(c: &Core, rec: &BoxRecord, caps: vk_cloud::Caps) -> Value {
    let (ws, panes) = cloud_panes(c, rec.task.as_deref());
    let mut v = json!({
        "box": rec.box_ref(),
        "provider": rec.provider,
        "id": rec.id,
        "name": rec.name,
        "state": if rec.state.is_empty() { "unknown" } else { rec.state.as_str() },
        "ownership": if rec.ownership.is_empty() { "orphaned" } else { rec.ownership.as_str() },
        "key": rec.key,
        "panes": panes,
        "sessions": rec.sessions,
        "created_at": rec.created_at,
        "last_activity_at": rec.last_activity_at,
        "unsynced": rec.unsynced,
        "caps": caps,
        "host_tag": rec.tags.as_ref().map(|t| t.host.clone()).unwrap_or_default(),
    });
    if let Some(t) = rec.task.as_deref().filter(|t| c.task(t).is_some()) {
        v["task"] = json!(t);
    }
    if let Some(w) = ws {
        v["workspace"] = json!(w);
    }
    if let Some(u) = &rec.url {
        v["url"] = json!(u);
    }
    v
}

pub fn view(server: &Server, rec: &BoxRecord) -> Value {
    let caps = caps_of(&rec.provider);
    server.with_core(|c| view_in(c, rec, caps))
}

/// Write `rec` and emit `cloud.box.changed` with its view.
pub fn save_record(server: &Server, rec: &BoxRecord) {
    let caps = caps_of(&rec.provider);
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.kv(
        K_BOX,
        &rec.box_ref(),
        Some(serde_json::to_string(rec).unwrap_or_default()),
    );
    let v = view_in(&c, rec, caps);
    tx.event("cloud.box.changed", json!({"box": rec.box_ref()}), v);
    let _ = server.commit(&mut c, tx);
}

/// Drop `rec` and emit `cloud.box.changed` with `state: destroyed`.
pub fn drop_record(server: &Server, rec: &BoxRecord) {
    let mut gone = rec.clone();
    gone.state = "destroyed".into();
    let caps = caps_of(&rec.provider);
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.kv(K_BOX, &rec.box_ref(), None);
    let v = view_in(&c, &gone, caps);
    tx.event("cloud.box.changed", json!({"box": rec.box_ref()}), v);
    let _ = server.commit(&mut c, tx);
}

/// This host's stable id (kv `cloud/host_id`, a ULID created once). Owner tags come from it.
pub fn host_id(server: &Server) -> String {
    let mut c = server.core.lock().unwrap();
    if let Ok(Some(v)) = c.store.kv_get(K_CLOUD, "host_id")
        && !v.is_empty()
    {
        return v;
    }
    let id = ulid();
    let mut tx = Tx::new();
    tx.m.kv(K_CLOUD, "host_id", Some(id.clone()));
    let _ = server.commit(&mut c, tx);
    id
}

// ---- config, credentials, errors ----------------------------------------------------------------

pub fn cloud_cfg() -> vk_cloud::CloudConfig {
    vk_cloud::CloudConfig::load()
}

/// The `[security] keychain` backend.
pub fn keychain() -> Result<vk_store::keychain::Keychain, RpcError> {
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    vk_cloud::auth::keychain(&cfg).map_err(|e| invalid(format!("[security] keychain: {e}")))
}

pub fn host_env(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

/// A provider by id, or `invalid_params` naming the known ones.
pub fn provider(id: &str) -> Result<Arc<dyn Provider>, RpcError> {
    let cfg = cloud_cfg();
    vk_cloud::provider(&cfg, id).ok_or_else(|| {
        let known: Vec<&str> = vk_cloud::providers(&cfg).iter().map(|p| p.id()).collect();
        invalid(format!("unknown cloud provider {id} ({})", known.join("|")))
    })
}

/// The `needs_auth` error every client turns into the sign-in prompt (spec 17 §5).
pub fn needs_auth(provider: &str, methods: &[AuthMethod], message: &str) -> RpcError {
    err(ErrorKind::PermissionDenied, message.to_string())
        .details(json!({"reason": "needs_auth", "provider": provider, "methods": methods}))
}

/// Map a provider error to the API's error kinds. Messages never carry a secret.
pub fn rpc_error(provider: &str, methods: &[AuthMethod], e: &CloudError) -> RpcError {
    use vk_cloud::ErrorKind as K;
    match e.kind {
        K::NeedsAuth => needs_auth(provider, methods, &e.message),
        K::Account => err(ErrorKind::PermissionDenied, e.message.clone())
            .details(json!({"reason": "account", "provider": provider})),
        K::NotFound => err(ErrorKind::NotFound, e.message.clone()),
        K::Conflict => err(ErrorKind::Conflict, e.message.clone()),
        K::Unsupported => err(ErrorKind::Unsupported, e.message.clone()),
        K::RateLimited => err(ErrorKind::RateLimited, e.message.clone()),
        K::InvalidParams => err(ErrorKind::InvalidParams, e.message.clone()),
        K::Unavailable => err(ErrorKind::RemoteUnavailable, e.message.clone()),
        K::Internal => err(ErrorKind::Internal, e.message.clone()),
    }
}

/// [`rpc_error`] for provider `p`.
pub fn map_err(p: &dyn Provider) -> impl Fn(CloudError) -> RpcError + '_ {
    move |e| rpc_error(p.id(), &p.auth_methods(), &e)
}

/// The provider and its resolved credential (config, keychain item, env), or `needs_auth`.
pub fn credential(
    _server: &Server,
    provider: &str,
) -> Result<(Arc<dyn Provider>, Secret), RpcError> {
    let cfg = cloud_cfg();
    let p = self::provider(provider)?;
    let kc = keychain()?;
    match vk_cloud::auth::resolve(p.as_ref(), &cfg, &kc, &host_env) {
        Ok(Some((s, _))) => Ok((p, s)),
        Ok(None) => Err(needs_auth(
            p.id(),
            &p.auth_methods(),
            &CloudError::needs_auth(p.id()).message,
        )),
        Err(e) => Err(rpc_error(p.id(), &p.auth_methods(), &e)),
    }
}

// ---- the context and its runner -----------------------------------------------------------------

/// The task's clone in the box (code isolation is always `clone`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudClone {
    /// Host repo (main checkout root) that sync fetches into.
    pub repo: PathBuf,
    /// Host worktree of the task.
    pub worktree: PathBuf,
    pub branch: String,
    pub base: String,
}

/// Everything the server keeps per cloud box (`BoxRunner::Cloud`).
#[derive(Debug, Clone)]
pub struct CloudCtx {
    pub provider: String,
    pub box_id: String,
    pub name: String,
    /// Box key (task id).
    pub key: String,
    /// `/workspace`.
    pub workdir: String,
    /// `vibeke` inside the box (`/vibeke/bin/vibeke`).
    pub bin: String,
    /// Prefix of every in-box path ([`vk_cloud::Provider::box_root`]; empty for real boxes).
    pub root: String,
    /// Host dir of the per-pane broker sockets (served into the box over the link).
    pub run_dir: PathBuf,
    pub clone: Option<CloudClone>,
    pub runner: CloudRunner,
}

impl CloudCtx {
    pub fn box_ref(&self) -> String {
        format!("{}/{}", self.provider, self.box_id)
    }
}

/// Builds pane argv: `vibeke cloud exec -i -t … <provider>/<id> -- <cmd>`.
#[derive(Debug, Clone)]
pub struct CloudRunner {
    pub provider: &'static str,
    pub box_ref: String,
    /// The host `vibeke` (runs `cloud exec`).
    pub host_bin: String,
    pub workdir: String,
    /// `<sbx>/<key>`: pane control dirs (session files) live in `.ctl` below it.
    pub root: PathBuf,
    pub run_dir: PathBuf,
    /// [`vk_cloud::Provider::box_root`] of the box.
    pub box_root: String,
    /// Non-secret per-exec env (credential paths rewritten to box paths).
    pub exec_env: Vec<(String, String)>,
    /// Secrets passed by name (`-e K`); the values are in the holder's env only.
    pub secrets: Vec<(String, String)>,
    /// Holder env of `vibeke cloud exec` (host basics, config/keychain location).
    pub cli_env: Vec<(String, String)>,
}

/// Host env `vibeke cloud exec` needs to find its config, keychain and provider: never the
/// user's other secrets. `extra` are the providers' credential variables (`SPRITES_TOKEN`, …),
/// passed only when set on the host, for credentials that come from the environment.
pub fn cli_env(host: &[(String, String)], extra: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in host {
        let keep = matches!(
            k.as_str(),
            "PATH"
                | "HOME"
                | "USER"
                | "LOGNAME"
                | "TMPDIR"
                | "LANG"
                | "XDG_RUNTIME_DIR"
                | "XDG_CONFIG_HOME"
                | "XDG_STATE_HOME"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
                | "HTTPS_PROXY"
                | "https_proxy"
                | "NO_PROXY"
                | "no_proxy"
                | "VIBEKE_CONFIG"
                | "VIBEKE_STATE_DIR"
                | "VIBEKE_RUNTIME_DIR"
                | "VIBEKE_CLOUD_FAKE_DIR"
        ) || k.starts_with("LC_")
            || extra.contains(k);
        if keep && !out.iter().any(|(x, _)| x == k) {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

/// The server's env for [`cli_env`]: its configured env first, then its process env.
fn server_env(server: &Server) -> Vec<(String, String)> {
    let mut env = server.opts.env.clone();
    for (k, v) in std::env::vars() {
        if !env.iter().any(|(x, _)| x == &k) {
            env.push((k, v));
        }
    }
    env
}

fn provider_env_vars() -> Vec<String> {
    vk_cloud::providers(&cloud_cfg())
        .iter()
        .flat_map(|p| p.auth_methods())
        .filter_map(|m| match m {
            AuthMethod::Env { var } => Some(var),
            _ => None,
        })
        .collect()
}

/// `vibeke cloud exec` argv (spec 17 §3.2). `pass` are env names whose values the exec takes
/// from its own env (`-e NAME`, secrets).
#[allow(clippy::too_many_arguments)]
pub fn exec_argv(
    host_bin: &str,
    tty: bool,
    workdir: Option<&str>,
    env: &[(String, String)],
    pass: &[String],
    session_file: Option<&Path>,
    box_ref: &str,
    cmd: &[String],
) -> Vec<String> {
    let mut v = vec![
        host_bin.to_string(),
        "cloud".into(),
        "exec".into(),
        "-i".into(),
    ];
    if tty {
        v.push("-t".into());
    }
    if let Some(w) = workdir {
        v.extend(["-w".into(), w.to_string()]);
    }
    for (k, val) in env {
        v.extend(["-e".into(), format!("{k}={val}")]);
    }
    for k in pass {
        v.extend(["-e".into(), k.clone()]);
    }
    if let Some(f) = session_file {
        v.extend(["--session-file".into(), f.to_string_lossy().into_owned()]);
    }
    v.push(box_ref.to_string());
    v.push("--".into());
    v.extend(cmd.iter().cloned());
    v
}

/// The bridge link: `vibeke cloud exec -i <box> -- /vibeke/bin/vibeke sandbox bridge …`.
pub fn link_argv(host_bin: &str, box_ref: &str, box_bin: &str, brokers: &str) -> Vec<String> {
    exec_argv(
        host_bin,
        false,
        None,
        &[],
        &[],
        None,
        box_ref,
        &[
            box_bin.to_string(),
            "sandbox".into(),
            "bridge".into(),
            "--brokers".into(),
            brokers.to_string(),
        ],
    )
}

/// The shell command git runs as `--upload-pack` / `--receive-pack` for the box repo.
pub fn git_service(host_bin: &str, box_ref: &str, service: &str) -> String {
    exec_argv(
        host_bin,
        false,
        None,
        &[],
        &[],
        None,
        box_ref,
        &["git".into(), service.into()],
    )
    .iter()
    .map(|a| sh_quote(a))
    .collect::<Vec<_>>()
    .join(" ")
}

/// Pane identity the box gets (never the pane token or host paths).
pub fn identity_env(env: &[(String, String)]) -> Vec<(String, String)> {
    env.iter()
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
        .collect()
}

impl Runner for CloudRunner {
    fn level(&self) -> vk_proto::model::IsolationLevel {
        vk_proto::model::IsolationLevel::Cloud
    }
    fn provider(&self) -> &'static str {
        self.provider
    }
    fn check(&self) -> Result<(), RunnerError> {
        Ok(())
    }
    fn prepare(&self, req: SpawnRequest) -> Result<PreparedSpawn, RunnerError> {
        let short = short_id(&req.pane_id);
        let mut env = self.exec_env.clone();
        for (k, v) in identity_env(&req.env) {
            vk_sandbox::env::set(&mut env, &k, v);
        }
        vk_sandbox::env::set(
            &mut env,
            "VIBEKE_SOCKET",
            format!("{}/{short}.sock", in_box(&self.box_root, BOX_BROKERS)),
        );
        let ctl = control_dir(&self.root, &req.pane_id)?;
        let cmd = box_command(&req.argv, BOX_SHELL);
        let pass: Vec<String> = self.secrets.iter().map(|(k, _)| k.clone()).collect();
        let argv = exec_argv(
            &self.host_bin,
            true,
            Some(&self.workdir),
            &env,
            &pass,
            Some(&ctl.join(SESSION_FILE)),
            &self.box_ref,
            &cmd,
        );
        let mut holder_env = self.cli_env.clone();
        for (k, v) in &self.secrets {
            vk_sandbox::env::set(&mut holder_env, k, v.clone());
        }
        Ok(PreparedSpawn {
            argv,
            cwd: req.cwd,
            env: holder_env,
            mounts: vec![],
            profile: None,
            broker_socket: Some(self.run_dir.join(format!("{short}.sock"))),
            visible_roots: vec![],
            policy: None,
        })
    }
}

/// Projected credential env (13 §8): paths under the projection dir become box paths under
/// `/vibeke/creds`; every other value is a secret passed by name.
pub fn split_projection_env(
    env: &[(String, String)],
    shared: &Path,
    creds: &str,
) -> (Vec<(String, String)>, Vec<(String, String)>) {
    let prefix = shared.to_string_lossy().into_owned();
    let mut exec_env = Vec::new();
    let mut secrets = Vec::new();
    for (k, v) in env {
        match v.strip_prefix(&prefix) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                exec_env.push((k.clone(), format!("{creds}{rest}")))
            }
            _ => secrets.push((k.clone(), v.clone())),
        }
    }
    (exec_env, secrets)
}

/// The debugging shell of `sandbox.shell` for a cloud box.
pub fn shell_argv(_server: &Server, c: &CloudCtx, term: &str) -> Vec<String> {
    exec_argv(
        &c.runner.host_bin,
        true,
        Some(&c.workdir),
        &[
            ("TERM".into(), term.to_string()),
            ("VIBEKE_SANDBOX_SHELL".into(), "1".into()),
        ],
        &[],
        None,
        &c.box_ref(),
        &[BOX_SHELL.into(), "-l".into()],
    )
}

/// An absolute in-box path below the box root ([`vk_cloud::Provider::box_root`]).
pub fn in_box(root: &str, path: &str) -> String {
    format!("{root}{path}")
}

/// The cloud context of a task box.
pub fn ctx(tb: &TaskBox) -> Option<&CloudCtx> {
    match &tb.runner {
        BoxRunner::Cloud(c) => Some(c),
        _ => None,
    }
}

fn mkdir_private(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
}

#[allow(clippy::too_many_arguments)]
fn make_ctx(
    server: &Server,
    provider: &'static str,
    box_id: &str,
    name: &str,
    key: &str,
    clone: Option<CloudClone>,
    projection_env: &[(String, String)],
    shared: &Path,
    root: String,
) -> CloudCtx {
    let (mut exec_env, secrets) =
        split_projection_env(projection_env, shared, &in_box(&root, BOX_CREDS));
    exec_env.push(("VIBEKE_ISOLATION".into(), "cloud".into()));
    exec_env.push(("VIBEKE_BIN".into(), in_box(&root, BOX_BIN)));
    let box_ref = format!("{provider}/{box_id}");
    let run_dir = super::container::run_dir(key);
    let runner = CloudRunner {
        provider,
        box_ref,
        host_bin: server.opts.bin.to_string_lossy().into_owned(),
        workdir: in_box(&root, BOX_WORKSPACE),
        root: sbx_root(key),
        run_dir: run_dir.clone(),
        box_root: root.clone(),
        exec_env,
        secrets,
        cli_env: cli_env(&server_env(server), &provider_env_vars()),
    };
    CloudCtx {
        provider: provider.to_string(),
        box_id: box_id.to_string(),
        name: name.to_string(),
        key: key.to_string(),
        workdir: in_box(&root, BOX_WORKSPACE),
        bin: in_box(&root, BOX_BIN),
        root,
        run_dir,
        clone,
        runner,
    }
}

/// A context for a recorded box without creating or contacting anything (box API, release of
/// a box whose task context is gone).
pub fn ctx_from_record(server: &Server, rec: &BoxRecord) -> Result<CloudCtx, RpcError> {
    let p = provider(&rec.provider)?;
    let clone = match (&rec.repo, &rec.worktree, &rec.branch) {
        (Some(r), Some(w), Some(b)) => Some(CloudClone {
            repo: r.into(),
            worktree: w.into(),
            branch: b.clone(),
            base: rec.base.clone().unwrap_or_default(),
        }),
        _ => None,
    };
    Ok(make_ctx(
        server,
        p.id(),
        &rec.id,
        &rec.name,
        &rec.key,
        clone,
        &[],
        Path::new("/nonexistent"),
        p.box_root(&rec.id),
    ))
}

// ---- in-box commands ----------------------------------------------------------------------------

/// Output of one in-box command.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub code: i32,
}

impl Captured {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn err_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

/// Largest output [`exec_capture`] collects: a handoff bundle (`vk_handoff::MAX_BUNDLE`) plus
/// slack for stderr and framing.
pub const MAX_CAPTURE: usize = vk_handoff::MAX_BUNDLE as usize + 10 * 1024 * 1024;

/// Run `argv` in the box without a terminal: `stdin` is sent followed by EOF, stdout and stderr
/// are collected until the command exits (or `timeout`).
pub async fn exec_capture(
    server: &Arc<Server>,
    c: &CloudCtx,
    argv: &[String],
    stdin: Vec<u8>,
    timeout: Duration,
) -> Result<Captured, RpcError> {
    let (p, cred) = credential(server, &c.provider)?;
    exec_on(p.as_ref(), &cred, &c.box_id, argv, stdin, timeout).await
}

async fn exec_on(
    p: &dyn Provider,
    cred: &Secret,
    id: &str,
    argv: &[String],
    stdin: Vec<u8>,
    timeout: Duration,
) -> Result<Captured, RpcError> {
    match tokio::time::timeout(timeout, exec_session(p, cred, id, argv, stdin)).await {
        Ok(r) => r,
        Err(_) => Err(err(
            ErrorKind::Timeout,
            format!(
                "{} in the box did not finish within {}s",
                argv.first().map(String::as_str).unwrap_or("command"),
                timeout.as_secs()
            ),
        )),
    }
}

async fn exec_session(
    p: &dyn Provider,
    cred: &Secret,
    id: &str,
    argv: &[String],
    stdin: Vec<u8>,
) -> Result<Captured, RpcError> {
    let req = ExecReq {
        argv: argv.to_vec(),
        env: vec![],
        cwd: None,
        tty: false,
        cols: 0,
        rows: 0,
        detachable: false,
    };
    let mut s = p.exec(cred, id, req).await.map_err(map_err(p))?;
    let input = s.input.clone();
    let feeder = tokio::spawn(async move {
        for chunk in stdin.chunks(64 * 1024) {
            if input.send(In::Data(chunk.to_vec())).await.is_err() {
                return;
            }
        }
        let _ = input.send(In::Eof).await;
    });
    let mut out = Captured::default();
    let r: Result<Captured, RpcError> = loop {
        match s.output.recv().await {
            Some(Out::Stdout(b)) => out.stdout.extend(b),
            Some(Out::Stderr(b)) => out.stderr.extend(b),
            Some(Out::Exit(code)) => {
                out.code = code;
                break Ok(out);
            }
            Some(Out::PortOpened { .. }) => {}
            Some(Out::Lost(m)) => {
                break Err(err(
                    ErrorKind::RemoteUnavailable,
                    format!("the connection to the box was lost: {m}"),
                ));
            }
            None => {
                break Err(err(
                    ErrorKind::RemoteUnavailable,
                    "the box command ended without an exit code",
                ));
            }
        }
        if out.stdout.len() + out.stderr.len() > MAX_CAPTURE {
            break Err(err(
                ErrorKind::Truncated,
                format!(
                    "the box command wrote more than {} MiB; output beyond that is not collected",
                    MAX_CAPTURE >> 20
                ),
            ));
        }
    };
    feeder.abort();
    r
}

fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.to_string()]
}

/// Write a file in the box (parents created).
pub async fn upload(
    server: &Arc<Server>,
    c: &CloudCtx,
    path: &str,
    data: Vec<u8>,
    mode: u32,
) -> Result<(), RpcError> {
    let (p, cred) = credential(server, &c.provider)?;
    p.write_file(&cred, &c.box_id, path, data, mode)
        .await
        .map_err(map_err(p.as_ref()))
}

// ---- creating a box (spec 17 §4) ----------------------------------------------------------------

/// Inputs for [`build`] (from `sandbox::prepare_box_opts`).
pub struct BuildIn<'a> {
    pub key: &'a str,
    pub task: Option<&'a str>,
    pub checkout: &'a Path,
    pub req: &'a mut IsoRequest,
    pub projection: &'a Projection,
    /// The projection's host dir (`<sbx>/shared`): uploaded to `/vibeke/creds`.
    pub shared: &'a Path,
    /// `false` = restore after a server restart: rebuild from the record, contact nothing.
    pub start: bool,
}

fn clone_info(server: &Server, task: &str, checkout: &Path) -> Result<CloudClone, RpcError> {
    let layout = vk_sandbox::GitLayout::detect_checked(
        checkout,
        &super::trusted_git_roots(server, Some(task)),
    )
    .map_err(|e| err(ErrorKind::PermissionDenied, e))?
    .ok_or_else(|| invalid(format!("{} is not a git checkout", checkout.display())))?;
    let branch = layout
        .branch
        .clone()
        .filter(|b| b != "HEAD")
        .ok_or_else(|| invalid("a cloud box needs a task branch (detached HEAD)"))?;
    let base = sync::resolve_commit(checkout, "HEAD")
        .map_err(|_| invalid("cannot resolve the checkout's HEAD"))?;
    let repo = vk_tasks::repo_root(checkout)
        .map(|r| r.root)
        .unwrap_or_else(|| layout.common_dir.parent().unwrap_or(checkout).to_path_buf());
    Ok(CloudClone {
        repo,
        worktree: checkout.to_path_buf(),
        branch,
        base,
    })
}

/// Build (and with `start`, create and bootstrap) the cloud context of task `key`.
pub async fn build(server: &Arc<Server>, i: BuildIn<'_>) -> Result<CloudCtx, RpcError> {
    let BuildIn {
        key,
        task,
        checkout,
        req,
        projection,
        shared,
        start,
    } = i;
    let Some(task) = task else {
        return Err(err(
            ErrorKind::Unsupported,
            "cloud isolation runs a task's panes: create a task with --isolate cloud, or send the agent with cloud.move",
        ));
    };
    let cfg = cloud_cfg();
    let (prov_id, box_id) = match req.cloud_box.as_deref() {
        Some(b) => {
            let (p, id) = parse_box_ref(b)?;
            (p, Some(id))
        }
        None => (
            req.provider
                .clone()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| cfg.default_provider.clone()),
            None,
        ),
    };
    let root = sbx_root(key);
    let run_dir = super::container::run_dir(key);
    if let Some(parent) = run_dir.parent().filter(|p| p.starts_with("/tmp")) {
        mkdir_private(parent).map_err(internal)?;
    }
    for d in [&root, &run_dir] {
        mkdir_private(d).map_err(internal)?;
    }
    let srv = server.clone();
    let (t2, co) = (task.to_string(), checkout.to_path_buf());
    let clone = tokio::task::spawn_blocking(move || clone_info(&srv, &t2, &co))
        .await
        .map_err(internal)??;
    if !start {
        // Restore: the record names the box; nothing is created or contacted.
        let p = provider(&prov_id)?;
        let id = box_id.ok_or_else(|| {
            err(
                ErrorKind::Unsupported,
                "this task's cloud box was never recorded",
            )
        })?;
        let rec = load_record(server, &format!("{prov_id}/{id}"));
        let name = rec.as_ref().map(|r| r.name.clone()).unwrap_or_default();
        return Ok(make_ctx(
            server,
            p.id(),
            &id,
            &name,
            key,
            Some(clone),
            &projection.env,
            shared,
            p.box_root(&id),
        ));
    }
    let (p, cred) = credential(server, &prov_id)?;
    let host = host_id(server);
    let tags = vk_cloud::naming::Tags::new(&host, key);
    // The task's own box from before (a bring-back kept or suspended it): use it again.
    let reuse = box_id.is_none().then(|| {
        list_records(server)
            .into_iter()
            .find(|r| r.key == key && r.provider == prov_id && r.ownership != "missing")
            .map(|r| r.id)
    });
    let found = match reuse.flatten() {
        Some(id) => match p.get(&cred, &id).await {
            Ok(rb) => Some(rb),
            Err(e) if e.kind == vk_cloud::ErrorKind::NotFound => None,
            Err(e) => return Err(map_err(p.as_ref())(e)),
        },
        None => None,
    };
    let (rb, created) = match (&box_id, found) {
        (_, Some(rb)) => (rb, false),
        (Some(id), None) => (p.get(&cred, id).await.map_err(map_err(p.as_ref()))?, false),
        (None, None) => {
            let pc = cfg.provider(&prov_id);
            let spec = vk_cloud::CreateSpec {
                name: vk_cloud::naming::box_name(&tags),
                tags: tags.clone(),
                template: pc.template.clone(),
                timeout_s: pc.timeout_s(),
                auto_pause: pc.auto_pause.unwrap_or(true),
                env: Default::default(),
            };
            (
                p.create(&cred, &spec).await.map_err(map_err(p.as_ref()))?,
                true,
            )
        }
    };
    // A box that sleeps (a suspended box taken up again) is woken before panes start in it.
    let mut rb = rb;
    if !created
        && !matches!(
            rb.state,
            vk_cloud::BoxState::Running | vk_cloud::BoxState::Creating
        )
    {
        p.resume(&cred, &rb.id).await.map_err(map_err(p.as_ref()))?;
        if let Ok(x) = p.get(&cred, &rb.id).await {
            rb = x;
        }
    }
    let box_ref = format!("{}/{}", p.id(), rb.id);
    req.provider = Some(p.id().to_string());
    req.cloud_box = Some(box_ref.clone());
    let prior = load_record(server, &box_ref);
    let c = make_ctx(
        server,
        p.id(),
        &rb.id,
        &rb.name,
        key,
        Some(clone.clone()),
        &projection.env,
        shared,
        p.box_root(&rb.id),
    );
    let bootstrapped = prior.as_ref().is_some_and(|r| r.key == key);
    if (created || !bootstrapped)
        && let Err(e) = bootstrap(server, p.as_ref(), &cred, &c, shared).await
    {
        if created {
            let _ = p.destroy(&cred, &rb.id).await;
        }
        return Err(e);
    }
    let now = vk_cloud::now_s();
    let rec = BoxRecord {
        provider: p.id().to_string(),
        id: rb.id.clone(),
        name: rb.name.clone(),
        key: key.to_string(),
        task: Some(task.to_string()),
        tags: rb.tags.clone().or(Some(tags)),
        created_at: if rb.created_at > 0 {
            rb.created_at
        } else {
            now
        },
        last_activity_at: now,
        state: rb.state.as_str().to_string(),
        ownership: "attached".into(),
        panes: vec![],
        unsynced: None,
        workdir: c.workdir.clone(),
        sessions: 0,
        url: rb.url.clone(),
        repo: Some(clone.repo.to_string_lossy().into_owned()),
        worktree: Some(clone.worktree.to_string_lossy().into_owned()),
        branch: Some(clone.branch.clone()),
        base: Some(clone.base.clone()),
    };
    save_record(server, &rec);
    Ok(c)
}

/// `uname -sm` → (os, arch) with the arch normalized to `x86_64` / `aarch64`.
pub fn parse_uname(s: &str) -> (String, String) {
    let mut it = s.split_whitespace();
    let os = it.next().unwrap_or_default().to_string();
    let arch = match it.next().unwrap_or_default() {
        "arm64" | "aarch64" => "aarch64",
        "amd64" | "x86_64" => "x86_64",
        other => other,
    }
    .to_string();
    (os, arch)
}

/// Where the Linux `vibeke` for a box of `arch` is expected.
pub fn linux_vibeke_path(home: &Path, arch: &str) -> PathBuf {
    home.join(".cache/vibeke/releases")
        .join(vk_proto::VERSION)
        .join(format!("vibeke-linux-{arch}"))
}

/// The `vibeke` binary for a box that runs `os`/`arch`.
fn box_vibeke(server: &Server, os: &str, arch: &str) -> Result<PathBuf, RpcError> {
    let home = server.sandbox.home();
    let host_os = match std::env::consts::OS {
        "macos" => "Darwin",
        "linux" => "Linux",
        o => o,
    };
    if os == "Linux" {
        let name = format!("vibeke-linux-{arch}");
        if let Some(d) = std::env::var_os("VIBEKE_ARTIFACT_DIR") {
            let p = PathBuf::from(d).join(&name);
            if p.is_file() {
                return Ok(p);
            }
        }
        let p = linux_vibeke_path(&home, arch);
        if p.is_file() {
            return Ok(p);
        }
        if arch == std::env::consts::ARCH
            && let Some(p) = super::container::linux_vibeke(&super::extras::cfg(server), &home)
        {
            return Ok(p);
        }
    } else if os == host_os && arch == std::env::consts::ARCH {
        // The box is a directory on this host (the `fake` provider): the host binary runs there.
        return Ok(server.opts.bin.clone());
    }
    // TODO(spec 17 §4 step 3): download the matching release asset and verify it with the
    // vk_remote minisign key.
    Err(err(
        ErrorKind::Unsupported,
        format!(
            "the box runs {os} {arch}, and no matching vibeke binary is on this host: expected {}",
            linux_vibeke_path(&home, arch).display()
        ),
    ))
}

/// Script that makes the in-box directories (with `sudo -n` when the box user cannot).
fn prep_script(root: &str) -> String {
    let q = |p: &str| sh_quote(&in_box(root, p));
    let (vk, ws, bin) = (q("/vibeke"), q(BOX_WORKSPACE), q("/vibeke/bin"));
    let (creds, home, brokers) = (q(BOX_CREDS), q(BOX_HOME), q(BOX_BROKERS));
    format!(
        "set -e\nfor d in {vk} {ws}; do\n  if [ ! -d \"$d\" ] || [ ! -w \"$d\" ]; then\n    mkdir -p \"$d\" 2>/dev/null || sudo -n mkdir -p \"$d\"\n    [ -w \"$d\" ] || sudo -n chown \"$(id -u):$(id -g)\" \"$d\"\n  fi\ndone\nmkdir -p {bin} {creds} {home} {brokers}\nchmod 700 {creds}\nuname -sm"
    )
}

/// Script that creates the box repo for `branch` (idempotent).
pub fn init_script(workdir: &str, branch: &str) -> String {
    let w = sh_quote(workdir);
    format!(
        "set -e\nif [ ! -d {w}/.git ]; then git init -q {w}; fi\ngit -C {w} config receive.denyCurrentBranch updateInstead\ngit -C {w} symbolic-ref HEAD {}",
        sh_quote(&format!("refs/heads/{branch}"))
    )
}

/// Script that checks out the pushed branch and sets the identity.
pub fn checkout_script(
    workdir: &str,
    branch: &str,
    name: Option<&str>,
    email: Option<&str>,
) -> String {
    let w = sh_quote(workdir);
    let mut s = vec![
        "set -e".to_string(),
        format!(
            "git -C {w} update-ref {} {}",
            sh_quote(&format!("refs/heads/{branch}")),
            sh_quote(&sync::host_ref(branch))
        ),
        format!("git -C {w} reset -q --hard"),
    ];
    if let Some(n) = name {
        s.push(format!("git -C {w} config user.name {}", sh_quote(n)));
    }
    if let Some(e) = email {
        s.push(format!("git -C {w} config user.email {}", sh_quote(e)));
    }
    s.join("\n")
}

fn git_config(dir: &Path, key: &str) -> Option<String> {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["config", key])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Regular files under `dir` (no symlinks followed), with their paths relative to it.
fn files_under(dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(m) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if m.is_dir() {
                stack.push(p);
            } else if m.is_file()
                && let Ok(rel) = p.strip_prefix(dir)
            {
                out.push((p.clone(), rel.to_path_buf()));
            }
        }
    }
    out.sort();
    out
}

/// The git services of the box repo, through `vibeke cloud exec`.
pub fn box_remote(c: &CloudCtx) -> BoxRemote {
    let box_ref = c.box_ref();
    BoxRemote {
        url: c.workdir.clone(),
        upload_pack: git_service(&c.runner.host_bin, &box_ref, "upload-pack"),
        receive_pack: git_service(&c.runner.host_bin, &box_ref, "receive-pack"),
        local: None,
    }
}

/// Steps 2–7 of spec 17 §4 on a new box: arch, `vibeke`, repo, push, checkout, identity,
/// projected credentials.
async fn bootstrap(
    server: &Arc<Server>,
    p: &dyn Provider,
    cred: &Secret,
    c: &CloudCtx,
    shared: &Path,
) -> Result<(), RpcError> {
    let t = Duration::from_secs(300);
    let fail = |what: &str, o: &Captured| {
        err(
            ErrorKind::Unsupported,
            format!("could not {what} in the cloud box: {}", o.err_text()),
        )
    };
    let o = exec_on(p, cred, &c.box_id, &sh(&prep_script(&c.root)), vec![], t).await?;
    if !o.ok() {
        return Err(fail("prepare /vibeke and /workspace", &o));
    }
    let (os, arch) = parse_uname(o.out().lines().last().unwrap_or_default());
    let bin = box_vibeke(server, &os, &arch)?;
    let data = tokio::fs::read(&bin).await.map_err(internal)?;
    p.write_file(cred, &c.box_id, &c.bin, data, 0o755)
        .await
        .map_err(map_err(p))?;
    let Some(cl) = c.clone.clone() else {
        return Ok(());
    };
    let o = exec_on(
        p,
        cred,
        &c.box_id,
        &sh(&init_script(&c.workdir, &cl.branch)),
        vec![],
        t,
    )
    .await?;
    if !o.ok() {
        return Err(fail("create the repository (does the box have git?)", &o));
    }
    let remote = box_remote(c);
    let (repo, branch) = (cl.repo.clone(), cl.branch.clone());
    tokio::task::spawn_blocking(move || sync::sync_push(&repo, &remote, &branch, &branch))
        .await
        .map_err(internal)?
        .map_err(|e| {
            err(
                ErrorKind::Unsupported,
                format!("could not push the task branch to the cloud box: {e}"),
            )
        })?;
    let wt = cl.worktree.clone();
    let (name, email) = tokio::task::spawn_blocking(move || {
        (git_config(&wt, "user.name"), git_config(&wt, "user.email"))
    })
    .await
    .map_err(internal)?;
    let o = exec_on(
        p,
        cred,
        &c.box_id,
        &sh(&checkout_script(
            &c.workdir,
            &cl.branch,
            name.as_deref(),
            email.as_deref(),
        )),
        vec![],
        t,
    )
    .await?;
    if !o.ok() {
        return Err(fail("check out the task branch", &o));
    }
    // Projected credentials (13 §8): the same relative layout as the container mount.
    for (host, rel) in files_under(shared) {
        let data = tokio::fs::read(&host).await.map_err(internal)?;
        let target = Path::new(&in_box(&c.root, BOX_CREDS)).join(&rel);
        p.write_file(cred, &c.box_id, &target.to_string_lossy(), data, 0o600)
            .await
            .map_err(map_err(p))?;
    }
    Ok(())
}

/// The cloud box of `task`, created (and the task moved to the `cloud` level) when it has none.
pub async fn ensure_for_task(
    server: &Arc<Server>,
    task: &str,
    provider: Option<&str>,
) -> Result<Arc<TaskBox>, RpcError> {
    let t = server
        .with_core(|c| c.task(task).cloned())
        .ok_or_else(|| crate::api::not_found("task", task))?;
    if let Some(b) = server.sandbox.get(&t.id) {
        return match &b.runner {
            BoxRunner::Cloud(c) if provider.is_none_or(|p| p == c.provider) => Ok(b),
            BoxRunner::Cloud(c) => Err(err(
                ErrorKind::Conflict,
                format!("task {} already runs in {}", t.handle, c.box_ref()),
            )),
            _ => Err(err(
                ErrorKind::Conflict,
                format!(
                    "task {} already runs at the {} level",
                    t.handle,
                    b.isolation.level.as_str()
                ),
            )),
        };
    }
    let checkout = t
        .worktree_path
        .clone()
        .map(PathBuf::from)
        .ok_or_else(|| invalid(format!("task {} has no checkout", t.handle)))?;
    let harnesses: Vec<String> = server.with_core(|c| {
        let mut v: Vec<String> = c
            .model
            .runs
            .iter()
            .filter(|r| r.ended_at_ms.is_none())
            .filter(|r| {
                c.pane(&r.pane)
                    .is_some_and(|p| t.workspace.as_deref() == Some(p.workspace.as_str()))
            })
            .map(|r| r.harness.clone())
            .collect();
        v.sort();
        v.dedup();
        v
    });
    let cfg = super::load_cfg();
    let req = IsoRequest {
        level: vk_proto::model::IsolationLevel::Cloud,
        network: cfg.network_profile(),
        harnesses,
        provider: provider.map(str::to_string),
        ..Default::default()
    };
    let b = super::prepare_box(server, &t.id, Some(&t.id), &checkout, req).await?;
    let mut c = server.core.lock().unwrap();
    if let Some(mut t2) = c.task(&t.id).cloned() {
        t2.isolation = b.isolation.clone();
        let mut tx = Tx::new();
        tx.event(
            "task.updated",
            json!({"task": t2.id}),
            json!({"isolation": t2.isolation.level.as_str()}),
        );
        tx.task(t2);
        let _ = server.commit(&mut c, tx);
    }
    drop(c);
    Ok(b)
}

// ---- the link -----------------------------------------------------------------------------------

/// Whether task `key` has a pane in its box (the link only runs then, so an idle box can sleep).
fn has_cloud_panes(server: &Server, key: &str) -> bool {
    server.with_core(|c| !cloud_panes(c, Some(key)).1.is_empty())
}

/// Keep the bridge link to the box up while the task has cloud panes: brokers of its panes
/// are served in the box. Reconnects with backoff (0.5 s doubling to 10 s). Abort to stop.
pub fn start_link(
    server: &Arc<Server>,
    c: &CloudCtx,
) -> (Arc<super::container::BoxLink>, tokio::task::JoinHandle<()>) {
    let link = Arc::new(super::container::BoxLink::default());
    let l2 = link.clone();
    let argv = link_argv(
        &c.runner.host_bin,
        &c.box_ref(),
        &c.bin,
        &in_box(&c.root, BOX_BROKERS),
    );
    let env = c.runner.cli_env.clone();
    let run_dir = c.run_dir.clone();
    let key = c.key.clone();
    let weak = Arc::downgrade(server);
    let h = tokio::spawn(async move {
        let mut backoff = Duration::from_millis(500);
        loop {
            let Some(srv) = weak.upgrade() else { return };
            let wanted = has_cloud_panes(&srv, &key);
            drop(srv);
            if !wanted {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            let started = Instant::now();
            let child = tokio::process::Command::new(&argv[0])
                .args(&argv[1..])
                .env_clear()
                .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn();
            if let Ok(mut child) = child
                && let (Some(si), Some(so)) = (child.stdin.take(), child.stdout.take())
            {
                let m = vk_remote::Mux::start(
                    so,
                    si,
                    "bridge",
                    Some(vk_remote::boxlink::host_acceptor(None, run_dir.clone())),
                );
                l2.connect(m.clone());
                tokio::select! {
                    _ = m.closed() => {}
                    _ = child.wait() => {}
                }
                l2.disconnect();
            }
            if started.elapsed() > Duration::from_secs(30) {
                backoff = Duration::from_millis(500);
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    });
    (link, h)
}

// ---- sync and unsynced work ---------------------------------------------------------------------

fn sync_err(e: vk_tasks::Error) -> RpcError {
    err(ErrorKind::Conflict, e.to_string())
}

/// `task.sync` for a cloud box: pull and push through the cloud exec git services.
pub async fn sync_task(
    server: &Arc<Server>,
    c: &CloudCtx,
    direction: &str,
    force: bool,
) -> Result<Vec<SyncOutcome>, RpcError> {
    if !matches!(direction, "pull" | "push" | "both") {
        return Err(invalid("direction is pull | push | both"));
    }
    let cl = c
        .clone
        .clone()
        .ok_or_else(|| invalid("this cloud box has no task clone"))?;
    // Fail early with needs_auth rather than inside git.
    credential(server, &c.provider)?;
    let remote = box_remote(c);
    let ns = short_id(&c.name);
    let mut out = Vec::new();
    if matches!(direction, "push" | "both") {
        let (repo, r, b) = (cl.repo.clone(), remote.clone(), cl.branch.clone());
        let o = tokio::task::spawn_blocking(move || sync::sync_push(&repo, &r, &b, &b))
            .await
            .map_err(internal)?
            .map_err(sync_err)?;
        out.push(o);
        let ff = exec_capture(
            server,
            c,
            &sh(&sync::box_ff_script(&c.workdir, &cl.branch)),
            vec![],
            Duration::from_secs(120),
        )
        .await?;
        if !ff.ok() {
            tracing::info!(stderr = %ff.err_text(), "cloud box did not fast-forward after push");
        }
    }
    if matches!(direction, "pull" | "both") {
        let (repo, r, b) = (cl.repo.clone(), remote.clone(), cl.branch.clone());
        let o = tokio::task::spawn_blocking(move || sync::sync_pull(&repo, &r, &b, &b, &ns, force))
            .await
            .map_err(internal)?
            .map_err(sync_err)?;
        out.push(o);
    }
    Ok(out)
}

fn record_sync(server: &Server, task: Option<&str>, key: &str, o: &[SyncOutcome]) {
    for x in o {
        emit(
            server,
            "task.synced",
            json!({"task": task, "sandbox": key}),
            json!({"direction": x.direction, "status": x.status, "commits": x.commits, "from": x.from, "to": x.to, "ref": x.reference}),
        );
    }
}

/// `task.sync {task}` when the task runs in a cloud box; `None` otherwise.
pub async fn task_sync(server: &Arc<Server>, p: &Value) -> Option<crate::api::R> {
    let t = crate::api::s(p, "task")?;
    let key = server
        .with_core(|c| c.task(t).map(|x| x.id.clone()))
        .unwrap_or_else(|| t.to_string());
    let tb = server.sandbox.get(&key)?;
    let c = ctx(&tb)?.clone();
    let dir = crate::api::s(p, "direction").unwrap_or("pull").to_string();
    let force = crate::api::b(p, "force").unwrap_or(false);
    Some(match sync_task(server, &c, &dir, force).await {
        Ok(o) => {
            record_sync(server, tb.task.as_deref(), &tb.key, &o);
            Ok(json!({"task": tb.task, "synced": o}))
        }
        Err(e) => Err(e),
    })
}

/// What the in-box report says ([`unsynced_script`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub missing: bool,
    pub head: Option<String>,
    /// Commits ahead of the host side ref (or the base).
    pub ahead: u32,
    pub dirty: u32,
    pub untracked: u32,
    pub stashes: u32,
}

/// In-box script printing `key=value` lines about the box repo.
/// Stash message of the leftovers a bring-back already carried to a host (`cloud_move`).
/// Those stashes are not unsynced work.
pub const BROUGHT_BACK_STASH: &str = "vibeke: brought back";

pub fn unsynced_script(workdir: &str, branch: Option<&str>, base: Option<&str>) -> String {
    let bb = sh_quote(BROUGHT_BACK_STASH);
    let w = sh_quote(workdir);
    let host = branch
        .map(|b| sh_quote(&sync::host_ref(b)))
        .unwrap_or_else(|| "''".into());
    let base = base
        .filter(|b| !b.is_empty())
        .map(sh_quote)
        .unwrap_or_else(|| "''".into());
    format!(
        "cd {w} 2>/dev/null && [ -d .git ] || {{ echo missing=1; exit 0; }}\n\
h=$(git rev-parse -q --verify HEAD 2>/dev/null || true)\n\
echo \"head=$h\"\n\
r={host}; b={base}\n\
if [ -n \"$r\" ] && git rev-parse -q --verify \"$r\" >/dev/null 2>&1; then a=$(git rev-list --count \"$r..HEAD\" 2>/dev/null || echo 0)\n\
elif [ -n \"$b\" ]; then a=$(git rev-list --count \"$b..HEAD\" 2>/dev/null || echo 0)\n\
elif [ -n \"$h\" ]; then a=$(git rev-list --count HEAD 2>/dev/null || echo 0)\n\
else a=0; fi\n\
echo \"ahead=$a\"\n\
echo \"dirty=$(git status --porcelain --untracked-files=no 2>/dev/null | wc -l)\"\n\
echo \"untracked=$(git ls-files --others --exclude-standard 2>/dev/null | wc -l)\"\n\
echo \"stashes=$(git stash list 2>/dev/null | grep -vc {bb})\"\n"
    )
}

pub fn parse_report(s: &str) -> Report {
    let mut r = Report::default();
    for line in s.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        let n = || v.parse::<u32>().unwrap_or(0);
        match k.trim() {
            "missing" => r.missing = v == "1",
            "head" => r.head = Some(v.to_string()).filter(|h| !h.is_empty()),
            "ahead" => r.ahead = n(),
            "dirty" => r.dirty = n(),
            "untracked" => r.untracked = n(),
            "stashes" => r.stashes = n(),
            _ => {}
        }
    }
    r
}

/// Commits reachable from `head` that no ref of the host repo has; `None` when the host does
/// not have `head` at all.
fn host_missing_commits(repo: &Path, head: &str) -> Option<u32> {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(sync::HARDEN)
            .args(args)
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
    };
    let has = git(&["cat-file", "-e", &format!("{head}^{{commit}}")])?;
    if !has.status.success() {
        return None;
    }
    let o = git(&["rev-list", "--count", head, "--not", "--all"])?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .flatten()
}

/// Combine the in-box report with what the host has: commits count only when no host ref has
/// them (pulled into the task branch or the mirror ref).
pub fn combine(r: &Report, host_missing: Option<Option<u32>>) -> Unsynced {
    let commits = match (&r.head, host_missing) {
        (None, _) => 0,
        // The host has the head: count what no host ref reaches.
        (Some(_), Some(Some(n))) => n,
        // The host lacks the head commit: at least one commit is only in the box.
        (Some(_), Some(None)) => r.ahead.max(1),
        // No host repo to compare with.
        (Some(_), None) => r.ahead,
    };
    Unsynced::from_counts(commits, r.dirty, r.untracked, r.stashes)
}

/// What the box has that the host does not (spec 17 §6.3).
pub async fn unsynced(server: &Arc<Server>, c: &CloudCtx) -> Result<Unsynced, RpcError> {
    let script = unsynced_script(
        &c.workdir,
        c.clone.as_ref().map(|cl| cl.branch.as_str()),
        c.clone.as_ref().map(|cl| cl.base.as_str()),
    );
    let o = exec_capture(server, c, &sh(&script), vec![], Duration::from_secs(120)).await?;
    if !o.ok() {
        return Err(err(
            ErrorKind::RemoteUnavailable,
            format!("could not inspect the box repository: {}", o.err_text()),
        ));
    }
    let r = parse_report(&o.out());
    if r.missing {
        return Ok(Unsynced::from_counts(0, 0, 0, 0));
    }
    let host = match (&r.head, c.clone.as_ref()) {
        (Some(h), Some(cl)) if cl.repo.is_dir() => {
            let (repo, h) = (cl.repo.clone(), h.clone());
            Some(
                tokio::task::spawn_blocking(move || host_missing_commits(&repo, &h))
                    .await
                    .map_err(internal)?,
            )
        }
        _ => None,
    };
    Ok(combine(&r, host))
}

// ---- release and destroy ------------------------------------------------------------------------

/// Stop serving task `key` from its box: link, context, kv `sandbox/<key>`. The box itself and
/// its record stay.
pub fn detach(server: &Server, key: &str) -> Option<Arc<TaskBox>> {
    super::stop_link(server, key);
    let removed = {
        let mut i = server.sandbox.inner.lock().unwrap();
        i.by_checkout.retain(|(_, k)| k != key);
        i.boxes.remove(key)
    };
    if let Some(b) = &removed {
        super::release_checkout(server, &b.checkout);
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.kv("sandbox", key, None);
    let _ = server.commit(&mut c, tx);
    removed
}

/// The unsynced-changes conflict (`cloud.box.destroy`, `release_task … destroy`).
pub fn unsynced_conflict(box_ref: &str, u: &Unsynced) -> RpcError {
    err(
        ErrorKind::Conflict,
        format!(
            "{box_ref} has work that is not on the host ({}); sync it or pass force",
            u.summary
        ),
    )
    .details(json!({"reason": "unsynced_changes", "unsynced": u}))
}

/// Destroy the box (no checks) and drop its record and host dirs.
pub async fn destroy_box(server: &Arc<Server>, c: &CloudCtx) -> Result<(), RpcError> {
    let (p, cred) = credential(server, &c.provider)?;
    p.destroy(&cred, &c.box_id)
        .await
        .map_err(map_err(p.as_ref()))?;
    let rec = load_record(server, &c.box_ref()).unwrap_or_else(|| BoxRecord {
        provider: c.provider.clone(),
        id: c.box_id.clone(),
        name: c.name.clone(),
        key: c.key.clone(),
        ..Default::default()
    });
    drop_record(server, &rec);
    if !c.key.is_empty() {
        let _ = std::fs::remove_dir_all(sbx_root(&c.key));
        let _ = std::fs::remove_dir_all(&c.run_dir);
    }
    Ok(())
}

/// Destroy after the guard: unsynced work needs `force` (spec 17 §6.3).
pub async fn destroy_checked(
    server: &Arc<Server>,
    c: &CloudCtx,
    force: bool,
) -> Result<Option<Unsynced>, RpcError> {
    let u = unsynced(server, c).await;
    match &u {
        Ok(x) if !x.is_clean() && !force => return Err(unsynced_conflict(&c.box_ref(), x)),
        Err(e) if !force => return Err(e.clone()),
        _ => {}
    }
    destroy_box(server, c).await?;
    Ok(u.ok())
}

/// Ask the provider to sleep the box and record its state.
pub async fn suspend_box(server: &Arc<Server>, c: &CloudCtx) -> Result<(), RpcError> {
    let (p, cred) = credential(server, &c.provider)?;
    p.suspend(&cred, &c.box_id)
        .await
        .map_err(map_err(p.as_ref()))?;
    let state = p
        .get(&cred, &c.box_id)
        .await
        .map(|b| b.state.as_str().to_string())
        .unwrap_or_else(|_| "unknown".into());
    if let Some(mut rec) = load_record(server, &c.box_ref()) {
        rec.state = state;
        save_record(server, &rec);
    }
    Ok(())
}

/// End a task's use of its cloud box (bring back, task close): pull its work, close the task's
/// box panes, stop serving the task from the box (new panes start on the host; the task's
/// isolation becomes host), then `after` = `keep | suspend | destroy`. Destroy refuses unsynced
/// work without `force`; a refused suspend or destroy keeps the box, and the result says why
/// (`action: kept`, `refused`).
pub async fn release_task(
    server: &Arc<Server>,
    task: &str,
    after: &str,
    force: bool,
) -> Result<Value, RpcError> {
    if !matches!(after, "keep" | "suspend" | "destroy") {
        return Err(invalid(format!(
            "after must be keep | suspend | destroy, got {after}"
        )));
    }
    let key = server
        .with_core(|c| c.task(task).map(|t| t.id.clone()))
        .unwrap_or_else(|| task.to_string());
    let c = match server.sandbox.get(&key).as_deref().and_then(ctx) {
        Some(c) => c.clone(),
        None => {
            let rec = record_for_key(server, &key)
                .ok_or_else(|| crate::api::not_found("cloud box of task", task))?;
            ctx_from_record(server, &rec)?
        }
    };
    release(server, &c, Some(&key), after, force).await
}

async fn release(
    server: &Arc<Server>,
    c: &CloudCtx,
    task: Option<&str>,
    after: &str,
    force: bool,
) -> Result<Value, RpcError> {
    let synced = match sync_task(server, c, "pull", false).await {
        Ok(o) => {
            record_sync(server, task, &c.key, &o);
            serde_json::to_value(&o).unwrap_or_default()
        }
        Err(e) => json!({"error": e.message}),
    };
    // The task leaves the box: its box panes close, new panes start on the host.
    if !c.key.is_empty() {
        let panes = server.with_core(|core| cloud_panes(core, Some(&c.key)).1);
        for pane in panes {
            server.close_pane(&pane);
        }
        detach(server, &c.key);
        let mut core = server.core.lock().unwrap();
        if let Some(mut t) = core.task(&c.key).cloned()
            && t.isolation.level == vk_proto::model::IsolationLevel::Cloud
        {
            t.isolation = vk_proto::model::Isolation::default();
            let mut tx = Tx::new();
            tx.event(
                "task.updated",
                json!({"task": t.id}),
                json!({"isolation": "host"}),
            );
            tx.task(t);
            let _ = server.commit(&mut core, tx);
        }
    }
    // keep | suspend | destroy; a refused suspend or destroy keeps the box and says why.
    let mut unsynced_now = None;
    let mut refused = Value::Null;
    let outcome: Result<&str, RpcError> = match after {
        "destroy" => match unsynced(server, c).await {
            Ok(u) if !u.is_clean() && !force => {
                let e = unsynced_conflict(&c.box_ref(), &u);
                unsynced_now = Some(u);
                Err(e)
            }
            Err(e) if !force => Err(e),
            u => {
                unsynced_now = u.ok();
                destroy_box(server, c).await.map(|()| "destroyed")
            }
        },
        "suspend" => suspend_box(server, c).await.map(|()| "suspended"),
        _ => Ok("kept"),
    };
    let action = match outcome {
        Ok(a) => a,
        Err(e) => {
            refused = json!({"kind": e.data.kind, "message": e.message, "details": e.data.details});
            "kept"
        }
    };
    if action != "destroyed"
        && let Some(mut rec) = load_record(server, &c.box_ref())
    {
        match unsynced_now.clone() {
            Some(u) => rec.unsynced = Some(u),
            None => {
                if let Ok(u) = unsynced(server, c).await {
                    rec.unsynced = Some(u);
                }
            }
        }
        save_record(server, &rec);
    }
    Ok(json!({
        "box": c.box_ref(), "task": task, "requested": after, "action": action,
        "refused": refused, "sync": synced, "unsynced": unsynced_now,
    }))
}

/// A cloud task closed (`sandbox::teardown`): apply `[cloud] on_task_close`.
pub fn on_task_close(server: &Arc<Server>, b: Arc<TaskBox>) {
    let Some(c) = ctx(&b).cloned() else {
        return;
    };
    let policy = cloud_cfg().on_task_close;
    let after = match policy.as_str() {
        "keep" | "suspend" | "destroy" => policy,
        // TODO(spec 17 §6.3): `ask` should open the task-close interaction (bring back / keep /
        // suspend / destroy). No such interaction exists for boxes yet, so ask suspends: nothing
        // is lost and the box stays listed.
        _ => "suspend".to_string(),
    };
    let Ok(h) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let srv = server.clone();
    let task = b.task.clone();
    h.spawn(async move {
        match release(&srv, &c, task.as_deref(), &after, false).await {
            Ok(r) if after == "destroy" && r["action"] != "destroyed" => {
                // Never forced: unsynced work keeps the box, asleep.
                tracing::info!(box_ref = %c.box_ref(), refused = %r["refused"], "cloud box kept at task close");
                let _ = suspend_box(&srv, &c).await;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(box_ref = %c.box_ref(), error = %e.message, "cloud box on_task_close")
            }
        }
    });
}

#[cfg(test)]
#[path = "sandbox_cloud_tests.rs"]
mod tests;
