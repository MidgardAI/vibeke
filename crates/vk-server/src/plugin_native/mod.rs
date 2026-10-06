//! Native plugin runtime (07 §7.1–7.6, 09 §6; spec-gap lane 3B).
//!
//! * **Identity and capabilities** ([`tokens`]): every native plugin process and argv
//!   invocation gets a capability-scoped token; calls made with it (over the socket after
//!   `client.hello {token}`, or over a process plugin's stdio) are checked against the approved
//!   capabilities before dispatch ([`authorize`], default deny,
//!   `vk_compat::native::caps::need`). A refusal is `permission_denied` plus a
//!   `plugin.capability_violation` event and audit record; every allowed mutating call is a
//!   `plugin.api_call` event with `actor.kind = plugin`.
//! * **Process plugins (Kind B)** ([`process`]): spawned by the server, JSON-RPC over stdio,
//!   `plugin.initialize {config, api}` → `{contributions}`, `plugin.shutdown` with a 5 s grace,
//!   restart policy with exponential backoff (at most 5 crashes in 10 minutes, then disabled for
//!   this server with a notification and `plugin.crashed {disabled: true}`); matching events are
//!   pushed as `events.event` notifications.
//! * **Argv actions and `[[on]]` hooks (Kind A)** ([`actions`]): argv, cwd = plugin dir, the
//!   `VIBEKE_PLUGIN_*` environment, 60 s token, captured output (last 4 KiB returned by
//!   `plugin.action`), a notification on non-zero exit, command records in `state.db`
//!   (`plugin_commands`), events `plugin.action_invoked` / `plugin.command_finished`.
//! * **Storage** ([`kv`]): `plugin.kv.*` in `state.db` (`plugin_kv`), 1 MiB per value, 64 MiB
//!   per plugin (`[plugins] kv_quota_bytes`).
//! * **UI contributions** ([`ui`]): `ui.contribute` and the `contributions` of
//!   `plugin.initialize`, validated against the `ui` capability, sanitized, debounced to 10 Hz
//!   per plugin (`ui.contributions_changed`), listed by `ui.contributions` and merged into
//!   `plugin.action.list` (palette commands, key bindings), `plugin.link_handler.list` and
//!   `compat.ui.state` for clients.
//! * **Registry observation and dev loop** ([`observe`]): committed registry generations are
//!   diffed into `plugin.installed/linked/unlinked/uninstalled/enabled/disabled/trust_changed`
//!   and `plugin.registry_observed {generation}` (Herdr entries included); linked native plugins
//!   are hot-restarted when their manifest, process files or `watch` globs change.
//! * **Sandbox** ([`launch`]): `sandbox = true` runs the process and commands under the OS
//!   sandbox generated from the capabilities (`vk_sandbox::native_plugin`), with the declared
//!   network hosts behind a per-plugin egress proxy; `[limits]` become `setrlimit` values.

pub mod actions;
pub mod api_install;
pub mod kv;
pub mod launch;
pub mod observe;
pub mod process;
pub mod tokens;
pub mod ui;

#[cfg(test)]
mod tests;

use crate::Server;
use crate::api::{Ctx, R, err, invalid};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;
use vk_compat::herdr::registry::{PluginDirs, Registry};
use vk_compat::native::manifest::Manifest;
use vk_compat::native::registry::{NativeEntry, NativeStatus, native_status};
use vk_proto::rpc::{ErrorKind, RpcError};

/// Ctx kind prefix of a native plugin connection: `native-plugin:<id>#<token hash>`.
pub const PLUGIN_KIND: &str = "native-plugin:";

pub const METHODS: &[(&str, bool)] = &[
    ("plugin.install", true),
    ("plugin.link", true),
    ("plugin.enable", true),
    ("plugin.disable", true),
    ("plugin.remove", true),
    ("plugin.consent", true),
    ("plugin.action", true),
    ("plugin.restart", true),
    ("plugin.kv.get", false),
    ("plugin.kv.set", true),
    ("plugin.kv.delete", true),
    ("plugin.kv.list", false),
    ("ui.contribute", true),
    ("ui.contributions", false),
    ("ui.pane.open", true),
];

/// Full-scope only (09 §5.2): registry changes, consent and plugin actions are user decisions;
/// KV and UI contributions need a plugin identity, which a pane never has.
pub const PANE_FORBIDDEN: &[&str] = &[
    "plugin.install",
    "plugin.link",
    "plugin.enable",
    "plugin.disable",
    "plugin.remove",
    "plugin.consent",
    "plugin.action",
    "plugin.restart",
    "plugin.kv.get",
    "plugin.kv.set",
    "plugin.kv.delete",
    "plugin.kv.list",
    "ui.contribute",
    "ui.pane.open",
];

// ---- per-server state -------------------------------------------------------------------------

#[derive(Default)]
pub struct State {
    /// Test override of the plugin directories (the process-wide defaults otherwise).
    pub dirs: Mutex<Option<PluginDirs>>,
    pub tokens: Mutex<HashMap<String, tokens::TokenInfo>>,
    pub procs: Mutex<HashMap<String, process::Handle>>,
    pub ui: Mutex<ui::Contribs>,
    /// Recent native command records (also persisted in `plugin_commands`).
    pub logs: Mutex<VecDeque<Value>>,
    /// Running argv invocations per plugin.
    pub running: Mutex<HashMap<String, usize>>,
    /// Crash times per plugin (restart budget) and plugins disabled for this server.
    pub crashes: Mutex<HashMap<String, Vec<Instant>>>,
    pub crash_disabled: Mutex<HashSet<String>>,
    /// Registry cache: (mtime, len) of plugins.json and the parsed registry.
    pub reg_cache: Mutex<Option<(RegKey, Registry)>>,
    /// Last observed registry snapshot (for event diffs).
    pub observed: Mutex<Option<observe::Snapshot>>,
    /// Dev-link fingerprints by plugin.
    pub fingerprints: Mutex<HashMap<String, u64>>,
}

/// `(mtime, size)` of `plugins.json`.
type RegKey = (Option<std::time::SystemTime>, u64);

static STATES: LazyLock<Mutex<HashMap<String, Arc<State>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn state(server: &Server) -> Arc<State> {
    STATES
        .lock()
        .unwrap()
        .entry(server.boot_id.clone())
        .or_default()
        .clone()
}

/// Plugin directories (the shared per-user ones unless a test set its own).
pub fn dirs(server: &Server) -> PluginDirs {
    state(server)
        .dirs
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(crate::compat::plugin_dirs)
}

/// Tests: use `d` for this server's plugin registry and plugin dirs.
pub fn set_dirs(server: &Server, d: PluginDirs) {
    *state(server).dirs.lock().unwrap() = Some(d);
    *state(server).reg_cache.lock().unwrap() = None;
}

pub fn vibeke_version() -> &'static str {
    vk_proto::VERSION
}

/// The registry, re-read only when `plugins.json` changed (mtime/size).
pub fn registry(server: &Server) -> Registry {
    let d = dirs(server);
    let key = std::fs::metadata(&d.registry)
        .map(|m| (m.modified().ok(), m.len()))
        .unwrap_or((None, 0));
    let st = state(server);
    let mut c = st.reg_cache.lock().unwrap();
    if let Some((k, r)) = c.as_ref()
        && *k == key
    {
        return r.clone();
    }
    let r = Registry::load_shared(&d).unwrap_or_default();
    *c = Some((key, r.clone()));
    r
}

/// Forget the cached registry (after a change made by this server).
pub fn registry_changed(server: &Server) {
    *state(server).reg_cache.lock().unwrap() = None;
}

/// An active native plugin, or why not.
pub fn active(server: &Server, id: &str) -> Result<(NativeEntry, Manifest), RpcError> {
    let reg = registry(server);
    let Some(e) = reg.native.get(id) else {
        return Err(err(ErrorKind::NotFound, format!("plugin not found: {id}"))
            .details(json!({"object": "plugin"})));
    };
    if state(server).crash_disabled.lock().unwrap().contains(id) {
        return Err(err(
            ErrorKind::Conflict,
            format!(
                "{id} was disabled after repeated crashes; `vibeke plugin enable {id}` or restart it"
            ),
        ));
    }
    match native_status(e, vibeke_version()) {
        (NativeStatus::Active, Some(m)) => Ok((e.clone(), m)),
        (st, _) => Err(err(
            ErrorKind::PermissionDenied,
            format!(
                "{id} is {}{}",
                st.as_str(),
                st.detail().map(|d| format!(" ({d})")).unwrap_or_default()
            ),
        )
        .details(json!({"status": st.as_str()}))),
    }
}

/// Is `id` a native plugin registration?
pub fn is_native(server: &Server, id: &str) -> bool {
    registry(server).native.contains_key(id)
}

pub fn now_ms() -> i64 {
    vk_store::now_ms()
}

pub fn actor(plugin: &str, invocation: Option<&str>) -> Value {
    json!({"kind": "plugin", "id": plugin, "invocation": invocation})
}

/// Commit one event (metadata only).
pub fn emit(server: &Server, kind: &str, subject: Value, actor: Value, data: Value) {
    let mut c = server.core.lock().unwrap();
    let mut tx = crate::core::Tx::new();
    tx.event_by(kind, subject, actor, data);
    if let Err(e) = server.commit(&mut c, tx) {
        tracing::warn!(error = %e, kind, "plugin event not recorded");
    }
}

pub fn is_plugin_kind(kind: &str) -> bool {
    kind.starts_with(PLUGIN_KIND)
}

/// The native plugin behind a connection, when it is one (and its token is still valid).
pub fn caller(server: &Server, ctx: &Ctx) -> Option<tokens::TokenInfo> {
    is_plugin_kind(&ctx.kind)
        .then(|| tokens::lookup(server, &ctx.kind))
        .flatten()
}

/// The plugin id a plugin-only method acts for, or `permission_denied`.
pub fn require_plugin(
    server: &Server,
    ctx: &Ctx,
    method: &str,
) -> Result<tokens::TokenInfo, RpcError> {
    caller(server, ctx).ok_or_else(|| {
        err(
            ErrorKind::PermissionDenied,
            format!("{method} is plugin-only: call it with a plugin token"),
        )
    })
}

fn denied(msg: impl Into<String>) -> RpcError {
    err(ErrorKind::PermissionDenied, msg).details(json!({"scope": "plugin"}))
}

/// Is the token's consent still the plugin's current one and the plugin active?
fn token_current(server: &Server, info: &tokens::TokenInfo) -> Result<(), String> {
    let reg = registry(server);
    let Some(e) = reg.native.get(&info.plugin) else {
        return Err(format!("{} is no longer registered", info.plugin));
    };
    if e.consent.as_ref().map(|c| c.consent_id.as_str()) != Some(info.consent_id.as_str()) {
        return Err(format!("{}'s consent changed or was revoked", info.plugin));
    }
    match native_status(e, vibeke_version()).0 {
        NativeStatus::Active => {}
        st => return Err(format!("{} is {}", info.plugin, st.as_str())),
    }
    if state(server)
        .crash_disabled
        .lock()
        .unwrap()
        .contains(&info.plugin)
    {
        return Err(format!(
            "{} was disabled after repeated crashes",
            info.plugin
        ));
    }
    Ok(())
}

/// `auth::authorize` hook: a plugin connection's token must still be valid (checked per call
/// and before every pushed event of a subscription).
pub fn authorize_live(server: &Server, ctx: &Ctx) -> Result<(), RpcError> {
    if !is_plugin_kind(&ctx.kind) {
        return Ok(());
    }
    let Some(info) = tokens::lookup(server, &ctx.kind) else {
        return Err(denied("plugin token expired or revoked"));
    };
    if let Err(why) = token_current(server, &info) {
        tokens::revoke_kind(server, &ctx.kind);
        return Err(denied(format!("plugin token no longer valid: {why}")));
    }
    Ok(())
}

/// `api::authorize` hook: capability check for plugin connections (default deny). Records
/// violations and mutating calls.
pub fn authorize(server: &Server, ctx: &Ctx, method: &str, p: &Value) -> Result<(), RpcError> {
    if !is_plugin_kind(&ctx.kind) {
        return Ok(());
    }
    authorize_live(server, ctx)?;
    let Some(info) = tokens::lookup(server, &ctx.kind) else {
        return Err(denied("plugin token expired or revoked"));
    };
    if let Err(why) = info.caps.check(method, p) {
        violation(server, &info, method, &why);
        return Err(denied(format!("capability_violation: {why}")));
    }
    if crate::session_api::is_mutating(method) && !matches!(method, "client.hello") {
        emit(
            server,
            "plugin.api_call",
            json!({"plugin": info.plugin}),
            actor(&info.plugin, Some(&info.invocation)),
            json!({"method": method, "token": info.kind.as_str()}),
        );
    }
    Ok(())
}

/// Record a capability violation: event (sync tier) and audit log (09 §11).
pub fn violation(server: &Server, info: &tokens::TokenInfo, method: &str, why: &str) {
    let data = json!({"method": method, "reason": why, "token": info.kind.as_str()});
    emit(
        server,
        "plugin.capability_violation",
        json!({"plugin": info.plugin}),
        actor(&info.plugin, Some(&info.invocation)),
        data.clone(),
    );
    crate::audit::record(
        server,
        "plugin.capability_violation",
        actor(&info.plugin, Some(&info.invocation)),
        json!({"plugin": info.plugin}),
        data,
    );
}

/// `client.hello {token}` hook: the ctx kind for a live plugin token.
pub fn hello(server: &Server, token: &str) -> Option<String> {
    tokens::kind_for(server, token)
}

/// A ctx for calls made with a plugin identity (stdio channel).
pub fn plugin_ctx(plugin: &str, kind: &str) -> Ctx {
    Ctx {
        client_id: format!("plugin:{plugin}"),
        kind: kind.to_string(),
        pane_scope: None,
        remote: false,
    }
}

/// `[plugins]` / `[plugins."<id>"]` value of `config.toml` (raw).
pub fn setting(id: &str, key: &str) -> Option<toml::Value> {
    let text = std::fs::read_to_string(vk_config::config_path()).ok()?;
    let t: toml::Table = text.parse().ok()?;
    let p = t.get("plugins")?.as_table()?;
    p.get(id)
        .and_then(|x| x.as_table())
        .and_then(|x| x.get(key))
        .or_else(|| p.get(key))
        .cloned()
}

/// Paths a plugin's data and config live in (created on demand).
pub fn plugin_paths(server: &Server, id: &str) -> (PathBuf, PathBuf) {
    let d = dirs(server);
    (d.state_dir(id), d.config_dir(id))
}

// ---- API --------------------------------------------------------------------------------------

/// Native-only methods of this module, plus the merged views of the shared `plugin.*` methods
/// (Herdr results from `compat::api` with native entries appended).
pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    let s = |k: &str| p.get(k).and_then(Value::as_str);
    let user_only = |what: &str| -> Result<(), RpcError> {
        if ctx.pane_scope.is_some() || is_plugin_kind(&ctx.kind) {
            Err(err(
                ErrorKind::PermissionDenied,
                format!("{what} needs the user (not a pane or a plugin)"),
            ))
        } else {
            Ok(())
        }
    };
    Some(match method {
        "plugin.install" => match user_only(method) {
            Ok(()) => Box::pin(api_install::install(server, p)).await,
            Err(e) => Err(e),
        },
        "plugin.link" => user_only(method).and_then(|()| api_install::link(server, p)),
        "plugin.enable" | "plugin.disable" => user_only(method)
            .and_then(|()| api_install::set_enabled(server, p, method == "plugin.enable")),
        "plugin.remove" => match user_only(method) {
            Ok(()) => api_install::remove(server, p).await,
            Err(e) => Err(e),
        },
        "plugin.consent" => match user_only(method) {
            Ok(()) => Box::pin(api_install::consent(server, p)).await,
            Err(e) => Err(e),
        },
        "plugin.restart" => match user_only(method) {
            Ok(()) => process::restart(server, s("plugin").unwrap_or_default()).await,
            Err(e) => Err(e),
        },
        "plugin.action" => actions::api_action(server, ctx, p, true).await,
        "plugin.action.run" => {
            let (plugin, _) = match actions::target(p) {
                Ok(x) => x,
                Err(e) => return Some(Err(e)),
            };
            if !is_native(server, &plugin) {
                // A Herdr plugin: the compat service runs it. A plugin caller may not run
                // another plugin's actions.
                if let Some(info) = caller(server, ctx) {
                    return Some(Err(denied(format!(
                        "{} may run its own actions only",
                        info.plugin
                    ))));
                }
                return None;
            }
            actions::api_action(server, ctx, p, false).await
        }
        "plugin.kv.get" | "plugin.kv.set" | "plugin.kv.delete" | "plugin.kv.list" => {
            kv::api(server, ctx, method, p)
        }
        "ui.contribute" => ui::api_contribute(server, ctx, p),
        "ui.contributions" => Ok(ui::list(server)),
        "ui.pane.open" => user_only(method).and_then(|()| ui::open_pane(server, ctx, p)),
        // Merged views of the shared methods.
        "plugin.list" => {
            let base = crate::compat::api(server, ctx, method, p).await?;
            base.map(|mut v| {
                if let Some(a) = v["plugins"].as_array_mut() {
                    a.extend(api_install::list(server));
                }
                v
            })
        }
        "plugin.action.list" => {
            let base = crate::compat::api(server, ctx, method, p).await?;
            match base {
                Ok(mut v) => {
                    let want = s("plugin");
                    if let Some(a) = v["actions"].as_array_mut() {
                        a.extend(actions::list(server, want));
                    }
                    let claimed: Vec<(String, String)> = v["keybindings"]
                        .as_array()
                        .map(|k| {
                            k.iter()
                                .filter(|x| x["installed"] == true)
                                .filter_map(|x| {
                                    Some((
                                        x["action"].as_str()?.to_string(),
                                        x["key"].as_str()?.to_string(),
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(k) = v["keybindings"].as_array_mut() {
                        k.extend(actions::keybindings(server, claimed));
                    }
                    Ok(v)
                }
                Err(e) if s("plugin").is_some_and(|id| is_native(server, id)) => {
                    let _ = e;
                    Ok(json!({
                        "actions": actions::list(server, s("plugin")),
                        "keybindings": actions::keybindings(server, vec![]),
                    }))
                }
                Err(e) => Err(e),
            }
        }
        "plugin.log.list" => {
            let base = crate::compat::api(server, ctx, method, p).await?;
            base.map(|mut v| {
                let own = caller(server, ctx).map(|i| i.plugin);
                let plugin = own.as_deref().or(s("plugin"));
                if let Some(a) = v["logs"].as_array_mut() {
                    if own.is_some() {
                        a.retain(|r| r["plugin_id"].as_str() == plugin);
                    }
                    a.extend(actions::logs(
                        server,
                        plugin,
                        p.get("limit").and_then(Value::as_u64),
                    ));
                }
                v
            })
        }
        "plugin.link_handler.list" => {
            let base = crate::compat::api(server, ctx, method, p).await?;
            base.map(|mut v| {
                if let Some(a) = v["handlers"].as_array_mut() {
                    a.extend(ui::link_handlers(server));
                }
                v
            })
        }
        "plugin.link.open" => {
            let plugin = s("plugin")?;
            if !is_native(server, plugin) {
                return None;
            }
            ui::open_link(server, ctx, p).await
        }
        "compat.ui.state" => {
            let base = crate::compat::api(server, ctx, method, p).await?;
            base.map(|mut v| {
                v["contributions"] = ui::list(server)["contributions"].clone();
                v
            })
        }
        _ => return None,
    })
}

/// Server start: registry observation (events, reconcile), autostart of process plugins, the
/// `[[on]]` hook dispatcher and the dev-link watcher.
pub fn start(server: &Arc<Server>) {
    actions::load_logs(server);
    let s = server.clone();
    tokio::spawn(async move { observe::run(s).await });
    let s = server.clone();
    tokio::spawn(async move { actions::hook_dispatcher(s).await });
}

/// `invalid_params` for a missing `plugin`.
pub fn plugin_param(p: &Value) -> Result<&str, RpcError> {
    p.get("plugin")
        .or_else(|| p.get("plugin_id"))
        .and_then(Value::as_str)
        .filter(|x| !x.is_empty())
        .ok_or_else(|| invalid("plugin is required"))
}
