//! Building a native plugin command (07 §7.2–7.3): argv (no shell), cwd = plugin root, a
//! relative program resolved against the root, the inherited environment minus pane/plugin
//! identity (or the scrubbed sandbox environment), the `VIBEKE_PLUGIN_*` values, `[limits]` as
//! `setrlimit`, and — with `sandbox = true` — the OS sandbox generated from the capabilities,
//! with declared network hosts behind a per-plugin egress proxy.

use super::{dirs, emit};
use crate::Server;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use vk_compat::native::manifest::Manifest;
use vk_compat::native::registry::NativeEntry;
use vk_sandbox::native_plugin::{self as nbox, NativeBox, ResourceLimits};

/// A command ready to spawn.
pub struct Launch {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub limits: ResourceLimits,
    /// Keeps the plugin's egress proxy alive as long as the command runs.
    pub proxy: Option<vk_sandbox::proxy::EgressProxy>,
    /// Generated sandbox profile (removed once the process started).
    pub profile: Option<PathBuf>,
}

/// Variables never inherited by a plugin command: pane identity, other plugins' identity and
/// Herdr invocation context.
fn inherited_ok(k: &str) -> bool {
    !(crate::run::PANE_IDENTITY_ENV.contains(&k)
        || k.starts_with("VIBEKE_PLUGIN_")
        || k.starts_with("VIBEKE_CONTEXT_")
        || k.starts_with("VIBEKE_HERDR_")
        || k.starts_with("HERDR_")
        || k == "VIBEKE_SOCKET"
        || k == "VIBEKE_SESSION")
}

/// `command[0]` relative to the plugin root when it names a path inside it.
pub fn resolve_argv(root: &Path, argv: &[String]) -> Vec<String> {
    let mut v = argv.to_vec();
    if let Some(first) = v.first_mut()
        && !first.starts_with('/')
        && first.contains('/')
    {
        *first = root.join(&*first).to_string_lossy().into_owned();
    }
    v
}

pub fn limits_of(m: &Manifest) -> ResourceLimits {
    ResourceLimits {
        memory_mb: m.limits.memory_mb,
        cpu_seconds: m.limits.cpu_seconds,
        open_files: m.limits.open_files,
    }
}

/// The base `VIBEKE_*` environment of a plugin command.
pub fn plugin_env(
    server: &Server,
    id: &str,
    token: &str,
    extra: &[(String, String)],
) -> Vec<(String, String)> {
    let (data, config) = super::plugin_paths(server, id);
    let _ = std::fs::create_dir_all(&data);
    let _ = std::fs::create_dir_all(&config);
    let mut v = vec![
        ("VIBEKE".to_string(), "1".to_string()),
        (
            "VIBEKE_SOCKET".into(),
            server.paths.socket().to_string_lossy().into_owned(),
        ),
        ("VIBEKE_SESSION".into(), server.paths.session.clone()),
        ("VIBEKE_PLUGIN_ID".into(), id.to_string()),
        ("VIBEKE_PLUGIN_TOKEN".into(), token.to_string()),
        (
            "VIBEKE_PLUGIN_DATA_DIR".into(),
            data.to_string_lossy().into_owned(),
        ),
        (
            "VIBEKE_PLUGIN_CONFIG_DIR".into(),
            config.to_string_lossy().into_owned(),
        ),
    ];
    v.extend(extra.iter().cloned());
    v
}

/// Prepare `argv` of plugin `e` for spawning. `label` names the generated profile.
pub async fn prepare(
    server: &Arc<Server>,
    e: &NativeEntry,
    m: &Manifest,
    argv: &[String],
    token: &str,
    extra: &[(String, String)],
    label: &str,
) -> Result<Launch, String> {
    if argv.is_empty() {
        return Err("empty command".into());
    }
    let argv = resolve_argv(&e.root, argv);
    let penv = plugin_env(server, &e.id, token, extra);
    let limits = limits_of(m);
    if !m.sandbox {
        let mut env: Vec<(String, String)> =
            std::env::vars().filter(|(k, _)| inherited_ok(k)).collect();
        for (k, v) in penv {
            env.retain(|(x, _)| *x != k);
            env.push((k, v));
        }
        return Ok(Launch {
            argv,
            env,
            cwd: e.root.clone(),
            limits,
            proxy: None,
            profile: None,
        });
    }
    // Sandboxed: profile from the capabilities.
    let caps = &m.capabilities;
    let mut proxy = None;
    let mut port = None;
    if !caps.network.is_empty() && !caps.network.iter().any(|h| h.trim() == "*") {
        let mut cfg = vk_sandbox::proxy::ProxyConfig::new(nbox::egress_policy(&caps.network));
        // A denied connection is a capability violation (network not declared).
        let (srv, plugin) = (server.clone(), e.id.clone());
        cfg.observer = Some(Arc::new(move |ev| {
            if let vk_sandbox::proxy::EgressEvent::Denied { host, port, reason } = ev {
                emit(
                    &srv,
                    "plugin.capability_violation",
                    json!({"plugin": plugin}),
                    super::actor(&plugin, None),
                    json!({"method": "network", "reason": format!("{host}:{port} not declared ({reason})")}),
                );
            }
        }));
        let p = vk_sandbox::proxy::EgressProxy::start(cfg, 0, None)
            .await
            .map_err(|err| format!("egress proxy: {err}"))?;
        port = Some(p.port);
        proxy = Some(p);
    }
    let d = dirs(server);
    let (data, config) = super::plugin_paths(server, &e.id);
    let home = crate::paths::home();
    let cfg_dir = vk_config::config_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut extra_read = vec![];
    if let Ok(exe) = std::env::current_exe() {
        extra_read.push(exe);
    }
    let nb = NativeBox {
        home: home.clone(),
        plugin_root: e.root.clone(),
        config_dir: config.clone(),
        state_dir: data.clone(),
        hidden: vec![
            crate::paths::state_root(),
            crate::paths::runtime_root(),
            cfg_dir,
            d.checkouts.clone(),
        ],
        extra_read,
        filesystem: nbox::resolve_filesystem(&caps.filesystem, &home, &data, &config),
        sockets: vec![server.paths.socket()],
        net: nbox::net_for(&caps.network, port),
        vibeke_bin: std::env::current_exe().ok(),
        profile_dir: server.paths.runtime.join("plugin-native").join("profiles"),
    };
    let prepared = nb.prepare(&argv, &e.root, penv, label)?;
    Ok(Launch {
        argv: prepared.argv,
        env: prepared.env,
        cwd: e.root.clone(),
        limits,
        proxy,
        profile: Some(prepared.profile),
    })
}

/// A `tokio` command for `l`: own process group, limits applied before exec.
pub fn command(l: &Launch) -> tokio::process::Command {
    let mut c = tokio::process::Command::new(&l.argv[0]);
    c.args(&l.argv[1..])
        .current_dir(&l.cwd)
        .env_clear()
        .envs(l.env.iter().cloned())
        .kill_on_drop(true);
    let limits = l.limits;
    // SAFETY: setsid and setrlimit are async-signal-safe.
    unsafe {
        c.pre_exec(move || {
            libc::setsid();
            limits.apply()
        });
    }
    c
}

/// Report a refused launch (sandbox unavailable, proxy failure) as an event.
pub fn launch_failed(server: &Server, plugin: &str, what: &str, why: &str) {
    emit(
        server,
        "plugin.launch_failed",
        json!({"plugin": plugin}),
        super::actor(plugin, None),
        json!({"what": what, "error": why}),
    );
}
