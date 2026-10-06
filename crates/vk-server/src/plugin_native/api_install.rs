//! Registry methods over the API (07 §2.16): `plugin.install`, `plugin.link`,
//! `plugin.enable` / `plugin.disable` / `plugin.remove`, `plugin.consent`, and the native rows
//! of `plugin.list`. All are full-scope only (never a pane or a plugin).
//!
//! * `plugin.install {source: path | owner/repo[/subdir][@ref], ref?, accept_capabilities?,
//!   trust?: scoped | herdr_legacy}` → `{plugin, requested_capabilities, status}`. A native
//!   plugin is installed only when `accept_capabilities` covers every requested item (item
//!   names, or `["*"]` after showing them); otherwise nothing is registered and the call fails
//!   with `permission_denied` `capabilities_not_accepted` and the requested items in
//!   `details`. Consent runs `[[build]]` (no token, socket or pane identity). A Herdr manifest
//!   is registered untrusted unless `trust: "herdr_legacy"` is given (an explicit legacy grant,
//!   07 §7.7); one that needs a `[[build]]` is trusted and built with `vibeke plugin trust`.
//! * `plugin.consent {plugin, accept_capabilities}` re-consents a native plugin after a
//!   widening update.

use super::{dirs, registry_changed};
use crate::Server;
use crate::api::{R, err, invalid};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use vk_compat::herdr::registry::{Registry, RegistryError};
use vk_compat::herdr::source;
use vk_compat::native::registry::{
    self as nreg, NativeStatus, consent_and_build, native_status,
};
use vk_proto::rpc::{ErrorKind, RpcError};

fn reg_err(e: RegistryError) -> RpcError {
    match e {
        RegistryError::NotFound(id) => {
            err(ErrorKind::NotFound, format!("plugin not found: {id}"))
                .details(json!({"object": "plugin"}))
        }
        RegistryError::Conflict(m) if m.starts_with("capabilities_not_accepted") => {
            err(ErrorKind::PermissionDenied, m).details(json!({"reason": "capabilities_not_accepted"}))
        }
        RegistryError::Conflict(m) => err(ErrorKind::Conflict, m),
        RegistryError::Manifest(m) => invalid(m.to_string()),
        other => crate::api::internal(other),
    }
}

fn accepted(p: &Value) -> Option<Vec<String>> {
    p.get("accept_capabilities").and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect()
    })
}

fn items_json(caps: &vk_compat::native::caps::Capabilities) -> Value {
    json!(caps.items())
}

fn audit(server: &Server, kind: &str, plugin: &str, data: Value) {
    crate::audit::record(
        server,
        kind,
        json!({"kind": "user", "client_kind": "api"}),
        json!({"plugin": plugin}),
        data,
    );
}

/// A source directory: an existing path, else a fetched `owner/repo` checkout. Returns the
/// directory plus the git origin data when fetched (and the fetch work dir to clean up).
async fn resolve_source(
    server: &Server,
    src: &str,
    git_ref: Option<&str>,
) -> Result<(PathBuf, Option<(source::Fetched, source::GitSource)>), RpcError> {
    let path = PathBuf::from(src);
    if path.exists() {
        return Ok((path, None));
    }
    let gs = source::parse(src, git_ref)
        .map_err(invalid)?
        .ok_or_else(|| invalid(format!("{src}: not a directory, manifest or owner/repo")))?;
    let parent = dirs(server).checkouts.join(".fetch");
    let gs2 = gs.clone();
    let fetched = tokio::task::spawn_blocking(move || source::fetch(&gs2, &source::base(), &parent))
        .await
        .map_err(crate::api::internal)?
        .map_err(|e| err(ErrorKind::RemoteUnavailable, format!("fetch failed: {e}")))?;
    Ok((fetched.plugin_dir.clone(), Some((fetched, gs))))
}

pub async fn install(server: &Arc<Server>, p: &Value) -> R {
    let src = p
        .get("source")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("source is required"))?;
    let git_ref = p.get("ref").and_then(Value::as_str);
    let trust = p.get("trust").and_then(Value::as_str).unwrap_or("scoped");
    if !matches!(trust, "scoped" | "herdr_legacy") {
        return Err(invalid("trust must be scoped or herdr_legacy"));
    }
    let (dir, fetched) = resolve_source(server, src, git_ref).await?;
    let cleanup = |f: &Option<(source::Fetched, source::GitSource)>| {
        if let Some((f, _)) = f {
            let _ = std::fs::remove_dir_all(&f.work);
        }
    };
    let d = dirs(server);
    if vk_compat::native::is_native(&dir) {
        let origin = match &fetched {
            Some((f, gs)) => nreg::git_origin(f, gs),
            None => nreg::local_origin(&dir, git_ref),
        };
        let staged = match nreg::stage(&d, &dir, origin) {
            Ok(s) => s,
            Err(e) => {
                cleanup(&fetched);
                return Err(reg_err(e));
            }
        };
        cleanup(&fetched);
        let m = staged.manifest.clone();
        if let Err(why) = m.compatible(super::vibeke_version(), vk_compat::herdr::current_platform()) {
            staged.discard();
            return Err(err(ErrorKind::Unsupported, why));
        }
        let requested = items_json(&m.capabilities);
        let acc = accepted(p).unwrap_or_default();
        let missing = m.capabilities.not_accepted(&acc);
        // A plugin that requests nothing needs no acceptance list.
        if !missing.is_empty() {
            staged.discard();
            return Err(err(
                ErrorKind::PermissionDenied,
                format!("capabilities_not_accepted: {}", missing.join(", ")),
            )
            .details(json!({
                "reason": "capabilities_not_accepted",
                "plugin": m.id,
                "requested_capabilities": requested,
                "max_risk": m.capabilities.max_risk().as_str(),
                "entrypoints": m.entrypoints(vk_compat::herdr::current_platform()),
            })));
        }
        let id = m.id.clone();
        let d2 = d.clone();
        let (e, _) = Registry::update(&d, move |r| r.native_install_staged(&d2, staged))
            .map_err(reg_err)?;
        let d3 = d.clone();
        let id2 = id.clone();
        let consent = tokio::task::spawn_blocking(move || consent_and_build(&d3, &id2, Some(acc.as_slice())))
            .await
            .map_err(crate::api::internal)?
            .map_err(reg_err)?;
        registry_changed(server);
        audit(
            server,
            "plugin.install",
            &id,
            json!({"kind": "native", "origin": e.origin.kind, "commit": e.origin.commit, "consent": consent.consent_id}),
        );
        let st = super::registry(server)
            .native
            .get(&id)
            .map(|e| native_status(e, super::vibeke_version()).0.as_str())
            .unwrap_or("unknown");
        return Ok(json!({"plugin": id, "kind": "native", "requested_capabilities": requested, "status": st}));
    }
    // A Herdr manifest.
    let staged = match &fetched {
        Some((f, gs)) => vk_compat::herdr::registry::stage_git(&d, f, gs),
        None => vk_compat::herdr::registry::stage_local(&d, &dir, git_ref),
    };
    cleanup(&fetched);
    let staged = staged.map_err(reg_err)?;
    let needs_build = !staged
        .manifest
        .build_on(vk_compat::herdr::current_platform())
        .is_empty();
    let d2 = d.clone();
    let (e, m) = Registry::update(&d, move |r| r.install_staged(&d2, staged)).map_err(reg_err)?;
    let mut status = "untrusted";
    if trust == "herdr_legacy" {
        if needs_build {
            registry_changed(server);
            return Err(err(
                ErrorKind::Conflict,
                format!(
                    "{} needs a [[build]]; review and build it with `vibeke plugin trust {} --legacy`",
                    e.id, e.id
                ),
            )
            .details(json!({"plugin": e.id, "status": "untrusted"})));
        }
        Registry::update(&d, |r| r.trust(&e.id)).map_err(reg_err)?;
        status = "active";
    }
    registry_changed(server);
    audit(
        server,
        "plugin.install",
        &e.id,
        json!({"kind": "herdr", "origin": e.origin.kind, "commit": e.origin.commit, "trust": trust}),
    );
    Ok(json!({
        "plugin": e.id,
        "kind": "herdr",
        "requested_capabilities": [],
        "entrypoints": m.entrypoints(vk_compat::herdr::current_platform()),
        "status": status,
    }))
}

pub fn link(server: &Arc<Server>, p: &Value) -> R {
    let path = p
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("path is required"))?;
    let d = dirs(server);
    let path = PathBuf::from(path);
    let out = if vk_compat::native::is_native(&path) {
        let (e, m) = Registry::update(&d, |r| r.native_link(&path)).map_err(reg_err)?;
        json!({"plugin": e.id, "kind": "native", "requested_capabilities": items_json(&m.capabilities)})
    } else {
        let (e, _) = Registry::update(&d, |r| r.link(&path)).map_err(reg_err)?;
        json!({"plugin": e.id, "kind": "herdr"})
    };
    registry_changed(server);
    Ok(out)
}

pub fn set_enabled(server: &Arc<Server>, p: &Value, on: bool) -> R {
    let id = super::plugin_param(p)?.to_string();
    let d = dirs(server);
    let reg = Registry::load_shared(&d).map_err(reg_err)?;
    if reg.native.contains_key(&id) {
        Registry::update(&d, |r| r.native_set_enabled(&id, on)).map_err(reg_err)?;
        if on {
            super::state(server).crash_disabled.lock().unwrap().remove(&id);
            super::state(server).crashes.lock().unwrap().remove(&id);
        }
    } else {
        Registry::update(&d, |r| r.set_enabled(&id, on)).map_err(reg_err)?;
    }
    registry_changed(server);
    Ok(json!({"plugin": id, "enabled": on}))
}

pub async fn remove(server: &Arc<Server>, p: &Value) -> R {
    let id = super::plugin_param(p)?.to_string();
    let purge = p.get("purge_data").and_then(Value::as_bool).unwrap_or(false);
    let d = dirs(server);
    let reg = Registry::load_shared(&d).map_err(reg_err)?;
    let kind = if reg.native.contains_key(&id) {
        super::process::stop(server, &id, "removed").await;
        super::tokens::revoke_plugin(server, &id);
        super::ui::clear(server, &id);
        let d2 = d.clone();
        Registry::update(&d, |r| r.native_remove(&d2, &id)).map_err(reg_err)?;
        if purge {
            let _ = server.with_core(|c| c.store.plugin_kv_clear(&id));
        }
        "native"
    } else {
        let e = reg.get(&id).map_err(|_| reg_err(RegistryError::NotFound(id.clone())))?;
        if e.managed {
            let d2 = d.clone();
            Registry::update(&d, |r| r.uninstall(&d2, &id)).map_err(reg_err)?;
        } else {
            Registry::update(&d, |r| r.unlink(&id)).map_err(reg_err)?;
        }
        "herdr"
    };
    registry_changed(server);
    audit(server, "plugin.remove", &id, json!({"kind": kind, "purge_data": purge}));
    Ok(json!({"plugin": id, "removed": true, "kind": kind}))
}

pub async fn consent(server: &Arc<Server>, p: &Value) -> R {
    let id = super::plugin_param(p)?.to_string();
    let d = dirs(server);
    let reg = Registry::load_shared(&d).map_err(reg_err)?;
    let e = reg.native_get(&id).map_err(reg_err)?.clone();
    let (m, _) = nreg::read_manifest(&e.root).map_err(reg_err)?;
    let acc = accepted(p).ok_or_else(|| {
        err(
            ErrorKind::PermissionDenied,
            "capabilities_not_accepted: pass accept_capabilities",
        )
        .details(json!({
            "reason": "capabilities_not_accepted",
            "requested_capabilities": items_json(&m.capabilities),
            "widened": e.consent.as_ref().map(|c| m.capabilities.widened_from(&c.capabilities)),
        }))
    })?;
    let d2 = d.clone();
    let id2 = id.clone();
    let c = tokio::task::spawn_blocking(move || consent_and_build(&d2, &id2, Some(acc.as_slice())))
        .await
        .map_err(crate::api::internal)?
        .map_err(reg_err)?;
    registry_changed(server);
    audit(server, "plugin.consent", &id, json!({"consent": c.consent_id}));
    Ok(json!({"plugin": id, "consent_id": c.consent_id, "capabilities": c.capabilities}))
}

/// Native rows for `plugin.list`.
pub fn list(server: &Server) -> Vec<Value> {
    let reg = super::registry(server);
    let d = dirs(server);
    reg.native
        .values()
        .map(|e| {
            let (st, m) = native_status(e, super::vibeke_version());
            let crashed = super::state(server).crash_disabled.lock().unwrap().contains(&e.id);
            let status = if crashed && st == NativeStatus::Active {
                "crashed".to_string()
            } else {
                st.as_str().to_string()
            };
            json!({
                "id": e.id,
                "plugin_id": e.id,
                "kind": m.as_ref().map(|m| m.kind()).unwrap_or("actions"),
                "native": true,
                "name": m.as_ref().and_then(|m| m.name.clone()),
                "version": m.as_ref().map(|m| m.version.clone()),
                "enabled": e.enabled,
                "status": status,
                "status_detail": st.detail(),
                "capabilities": m.as_ref().map(|m| m.capabilities.clone()),
                "consented": e.consent.as_ref().map(|c| json!({"consent_id": c.consent_id, "version": c.version, "granted_at": c.granted_at_ms})),
                "managed": e.managed,
                "dev": !e.managed,
                "origin": e.origin,
                "root": e.root,
                "source": e.origin.path,
                "sandbox": m.as_ref().map(|m| m.sandbox),
                "sandbox_available": m.as_ref().filter(|m| m.sandbox).map(|_| vk_sandbox::plugin::probe().is_ok()),
                "config_dir": d.config_dir(&e.id),
                "state_dir": d.state_dir(&e.id),
                "actions": m.as_ref().map(|m| m.actions_on(vk_compat::herdr::current_platform()).len()),
                "events": m.as_ref().map(|m| m.on.iter().map(|h| h.event.clone()).collect::<Vec<_>>()),
                "process": super::process::status(server, &e.id),
                "kv_quota": super::kv::quota(&e.id),
                "warnings": m.as_ref().map(|m| m.warnings.clone()),
            })
        })
        .collect()
}
