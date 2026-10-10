//! Cloud sign-in and box lifecycle API (spec 17 §5, §6.3): `cloud.providers`,
//! `cloud.auth.set|import|clear`, `cloud.box.list|suspend|resume|checkpoint|destroy|adopt|forget`
//! and `cloud.prune`. Moves (`cloud.move`, spec 17 §7) live in `cloud_move.rs`.
//!
//! Every `cloud.*` method is the user's: pane scope is refused (`api::PANE_FORBIDDEN_PREFIXES`).
//! A method that needs a provider and has no usable credential fails with `permission_denied`
//! and `details {reason: "needs_auth", provider, methods}`; clients show the sign-in prompt,
//! call `cloud.auth.set` or `cloud.auth.import`, and retry once. The `token` of
//! `cloud.auth.set` is verified, stored in the keychain item `vibeke/cloud/<provider>` and
//! never echoed, logged or put in an event.

use crate::Server;
use crate::api::{Ctx, R, b, err, invalid, not_found, req, s};
use crate::core::Tx;
use crate::sandbox::cloud::{
    self as cl, BoxRecord, credential, ctx_from_record, keychain, list_records, load_record,
    parse_box_ref, view,
};
use serde_json::{Value, json};
use std::sync::Arc;
use vk_cloud::{AuthMethod, Provider, Secret};
use vk_proto::rpc::{ErrorKind, RpcError};

pub const METHODS: &[(&str, bool)] = &[
    ("cloud.providers", false),
    ("cloud.auth.set", true),
    ("cloud.auth.import", true),
    ("cloud.auth.clear", true),
    ("cloud.box.list", false),
    ("cloud.box.suspend", true),
    ("cloud.box.resume", true),
    ("cloud.box.checkpoint", true),
    ("cloud.box.destroy", true),
    ("cloud.box.adopt", true),
    ("cloud.box.forget", true),
    ("cloud.prune", true),
];

/// Params that carry a secret: never logged, audited or echoed.
pub const SECRET_PARAMS: &[(&str, &str)] = &[("cloud.auth.set", "token")];

/// Ownership values `cloud.prune` may act on.
const PRUNABLE: &[&str] = &["orphaned", "idle", "missing"];

pub const DEFS: &str = r##"
CloudCaps = {resize: bool, reattach: bool, explicit_suspend: bool, keeps_memory: bool, checkpoints: bool, port_urls: bool, max_runtime_s: int}
CloudAuthMethod = {kind: paste_token, label: string, help_url: string, hint: string} | {kind: import, source: string, label: string} | {kind: env, var: string}
CloudUnsynced = {commits: int, dirty: int, untracked: int, summary: string, unknown?: bool}
CloudBoxView = {box: string, provider: string, id: string, name: string, state: string, ownership: attached|idle|orphaned|foreign|missing, key: string, task?: string, workspace?: string, panes: [string], sessions: int, created_at: int, last_activity_at: int, url?: string, unsynced: CloudUnsynced|null, caps: CloudCaps, host_tag: string}
CloudProvider = {id: string, label: string, caps: CloudCaps, default: bool, auth: {state: missing|ok|invalid, source?: string, account?: string, error?: string}, methods: [CloudAuthMethod]}
"##;

pub const SHAPES: &str = r##"
# --- cloud sandboxes (spec 17 §5, §6.3): full scope only; a method without a usable credential fails permission_denied with details {reason: needs_auth, provider, methods} ---
# every provider with its sign-in state; verify checks the credential with the provider
cloud.providers :: {verify?: bool = false} => {providers: [CloudProvider]}
# verifies the token, then stores it in the keychain item vibeke/cloud/<provider>; a rejected token fails needs_auth
cloud.auth.set :: {provider: string, token: string} => {provider: string, account: string}
# imports an existing login on the host (a source from the provider's import methods); not_found when it has none
cloud.auth.import :: {provider: string, source: string} => {provider: string, account: string}
cloud.auth.clear :: {provider: string} => {provider: string, cleared: bool, boxes_running: int}
# boxes this host knows; refresh lists the providers first (the reconciler)
cloud.box.list :: {provider?: string, ownership?: attached|idle|orphaned|foreign|missing, refresh?: bool = false}
  => {boxes: [CloudBoxView], errors: [{provider: string, kind: string, message: string}]}
cloud.box.suspend :: {box: string} => CloudBoxView
cloud.box.resume :: {box: string} => CloudBoxView
cloud.box.checkpoint :: {box: string, note?: string} => {box: string, checkpoint: string}
# closes the box's panes first; unsynced work without force fails conflict, details {reason: unsynced_changes, unsynced}
cloud.box.destroy :: {box: string, force?: bool = false} => {box: string, destroyed: true}
# not available yet: unsupported
cloud.box.adopt :: {box: string} => {box: string, task: string}
# drops the record of a box the provider no longer lists (ownership missing)
cloud.box.forget :: {box: string} => {box: string}
cloud.prune :: {provider?: string, ownership?: [orphaned|idle|missing], dry_run?: bool = false, force?: bool = false}
  => {candidates: [CloudBoxView], destroyed: [string], skipped: [{box: string, reason: string}]}
"##;

pub const EVENTS: &str = r##"
cloud.box.changed :: {box: string} => CloudBoxView
cloud.auth.changed :: {provider: string} => {state: missing|ok|invalid, account?: string}
"##;

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if !METHODS.iter().any(|(m, _)| *m == method) {
        return None;
    }
    if ctx.pane_scope.is_some() {
        return Some(Err(err(
            ErrorKind::PermissionDenied,
            format!("{method} is not allowed from a pane"),
        )
        .details(json!({"scope": "pane"}))));
    }
    Some(match method {
        "cloud.providers" => providers(server, p).await,
        "cloud.auth.set" => auth_set(server, p).await,
        "cloud.auth.import" => auth_import(server, p).await,
        "cloud.auth.clear" => auth_clear(server, p),
        "cloud.box.list" => box_list(server, p).await,
        "cloud.box.suspend" => box_power(server, p, true).await,
        "cloud.box.resume" => box_power(server, p, false).await,
        "cloud.box.checkpoint" => box_checkpoint(server, p).await,
        "cloud.box.destroy" => box_destroy(server, p).await,
        "cloud.box.adopt" => box_adopt(server, p).await,
        "cloud.box.forget" => box_forget(server, p),
        "cloud.prune" => prune(server, p).await,
        _ => return None,
    })
}

fn emit(server: &Server, kind: &str, subject: Value, data: Value) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(kind, subject, data);
    let _ = server.commit(&mut c, tx);
}

fn account_key(provider: &str) -> String {
    format!("account:{provider}")
}

fn stored_account(server: &Server, provider: &str) -> Option<String> {
    server
        .with_core(|c| {
            c.store
                .kv_get(cl::K_CLOUD, &account_key(provider))
                .ok()
                .flatten()
        })
        .filter(|a| !a.is_empty())
}

fn set_account(server: &Server, provider: &str, account: Option<&str>) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.kv(
        cl::K_CLOUD,
        &account_key(provider),
        account.map(str::to_string),
    );
    let _ = server.commit(&mut c, tx);
}

fn auth_changed(server: &Server, provider: &str, state: &str, account: Option<&str>) {
    let mut data = json!({"state": state});
    if let Some(a) = account {
        data["account"] = json!(a);
    }
    emit(
        server,
        "cloud.auth.changed",
        json!({"provider": provider}),
        data,
    );
}

fn source_str(s: &vk_cloud::auth::Source) -> String {
    match s {
        vk_cloud::auth::Source::Config => "config".into(),
        vk_cloud::auth::Source::Keychain => "keychain".into(),
        vk_cloud::auth::Source::Env(v) => format!("env:{v}"),
    }
}

/// `{state, source?, account?, error?}` of one provider.
async fn auth_state(
    server: &Server,
    p: &dyn Provider,
    cfg: &vk_cloud::CloudConfig,
    kc: &vk_store::keychain::Keychain,
    verify: bool,
) -> Value {
    use vk_cloud::ErrorKind as K;
    match vk_cloud::auth::resolve(p, cfg, kc, &cl::host_env) {
        Ok(None) => json!({"state": "missing"}),
        Ok(Some((secret, src))) => {
            let mut v = json!({"state": "ok", "source": source_str(&src)});
            if verify {
                match p.verify(&secret).await {
                    Ok(a) => {
                        set_account(server, p.id(), Some(&a.label));
                        v["account"] = json!(a.label);
                    }
                    Err(e) if e.kind == K::NeedsAuth => {
                        v["state"] = json!("invalid");
                        v["error"] = json!(e.message);
                    }
                    Err(e) => {
                        v["error"] = json!(e.message);
                    }
                }
            }
            if v.get("account").is_none()
                && v["state"] == "ok"
                && let Some(a) = stored_account(server, p.id())
            {
                v["account"] = json!(a);
            }
            v
        }
        Err(e) if e.kind == K::NeedsAuth => {
            json!({"state": "invalid", "source": "config", "error": e.message})
        }
        Err(e) => json!({"state": "missing", "error": e.message}),
    }
}

async fn providers(server: &Arc<Server>, p: &Value) -> R {
    let verify = b(p, "verify").unwrap_or(false);
    let cfg = cl::cloud_cfg();
    let kc = keychain()?;
    let mut out = Vec::new();
    for prov in vk_cloud::providers(&cfg) {
        let auth = auth_state(server, prov.as_ref(), &cfg, &kc, verify).await;
        out.push(json!({
            "id": prov.id(),
            "label": prov.label(),
            "caps": prov.caps(),
            "default": prov.id() == cfg.default_provider,
            "auth": auth,
            "methods": prov.auth_methods(),
        }));
    }
    Ok(json!({"providers": out}))
}

/// Verify `secret` with `prov`, store it, record the account and announce the change.
async fn sign_in(server: &Server, prov: &dyn Provider, secret: &Secret) -> R {
    let account = prov.verify(secret).await.map_err(cl::map_err(prov))?.label;
    let kc = keychain()?;
    vk_cloud::auth::store(&kc, prov.id(), secret).map_err(cl::map_err(prov))?;
    set_account(server, prov.id(), Some(&account));
    auth_changed(server, prov.id(), "ok", Some(&account));
    Ok(json!({"provider": prov.id(), "account": account}))
}

async fn auth_set(server: &Arc<Server>, p: &Value) -> R {
    let prov = cl::provider(req(p, "provider")?)?;
    let secret = Secret::new(req(p, "token")?);
    if secret.is_empty() {
        return Err(invalid("token is empty"));
    }
    sign_in(server, prov.as_ref(), &secret).await
}

async fn auth_import(server: &Arc<Server>, p: &Value) -> R {
    let prov = cl::provider(req(p, "provider")?)?;
    let source = req(p, "source")?;
    let known = prov
        .auth_methods()
        .iter()
        .any(|m| matches!(m, AuthMethod::Import { source: src, .. } if src == source));
    if !known {
        return Err(invalid(format!(
            "{} has no import source {source}",
            prov.id()
        )));
    }
    let secret = prov
        .import(source)
        .await
        .map_err(cl::map_err(prov.as_ref()))?
        .ok_or_else(|| {
            err(
                ErrorKind::NotFound,
                format!("no {source} login found on this host"),
            )
        })?;
    sign_in(server, prov.as_ref(), &secret).await
}

fn running(state: &str) -> bool {
    matches!(state, "running" | "creating" | "warm")
}

fn auth_clear(server: &Arc<Server>, p: &Value) -> R {
    let prov = cl::provider(req(p, "provider")?)?;
    let kc = keychain()?;
    let cleared = vk_cloud::auth::clear(&kc, prov.id()).map_err(cl::map_err(prov.as_ref()))?;
    set_account(server, prov.id(), None);
    let boxes_running = list_records(server)
        .iter()
        .filter(|r| r.provider == prov.id() && running(&r.state))
        .count();
    // A configured or environment credential may still apply.
    let cfg = cl::cloud_cfg();
    let state = match vk_cloud::auth::resolve(prov.as_ref(), &cfg, &kc, &cl::host_env) {
        Ok(Some(_)) => "ok",
        _ => "missing",
    };
    auth_changed(server, prov.id(), state, None);
    Ok(json!({"provider": prov.id(), "cleared": cleared, "boxes_running": boxes_running}))
}

async fn box_list(server: &Arc<Server>, p: &Value) -> R {
    let provider = s(p, "provider");
    let ownership = s(p, "ownership");
    if let Some(pid) = provider {
        credential(server, pid)?;
    }
    let errors = if b(p, "refresh").unwrap_or(false) {
        crate::cloud_reconcile::run(server, provider).await
    } else {
        vec![]
    };
    let boxes: Vec<Value> = list_records(server)
        .into_iter()
        .filter(|r| provider.is_none_or(|x| r.provider == x))
        .filter(|r| ownership.is_none_or(|o| r.ownership == o))
        .map(|r| view(server, &r))
        .collect();
    Ok(json!({"boxes": boxes, "errors": errors}))
}

/// A record for a box this host has none for, after checking that Vibeke made it.
pub(crate) fn record_from_remote(rb: &vk_cloud::RemoteBox) -> BoxRecord {
    BoxRecord {
        provider: rb.provider.clone(),
        id: rb.id.clone(),
        name: rb.name.clone(),
        tags: rb.tags.clone(),
        created_at: rb.created_at,
        last_activity_at: rb.last_active_at,
        state: rb.state.as_str().to_string(),
        ownership: "orphaned".into(),
        url: rb.url.clone(),
        workdir: vk_sandbox::container::BOX_WORKSPACE.into(),
        ..Default::default()
    }
}

/// The record of `box_ref`, or one made from the provider's view of it. Boxes Vibeke did not
/// create (no owner tags) are refused.
async fn record_or_remote(
    server: &Server,
    prov: &dyn Provider,
    cred: &Secret,
    box_ref: &str,
    id: &str,
) -> Result<BoxRecord, RpcError> {
    if let Some(r) = load_record(server, box_ref) {
        return Ok(r);
    }
    let rb = prov.get(cred, id).await.map_err(cl::map_err(prov))?;
    if rb.tags.is_none() {
        return Err(err(
            ErrorKind::PermissionDenied,
            format!("{box_ref} was not created by Vibeke; it is left alone"),
        ));
    }
    Ok(record_from_remote(&rb))
}

async fn box_power(server: &Arc<Server>, p: &Value, suspend: bool) -> R {
    let box_ref = req(p, "box")?;
    let (pid, id) = parse_box_ref(box_ref)?;
    let (prov, cred) = credential(server, &pid)?;
    let mut rec = record_or_remote(server, prov.as_ref(), &cred, box_ref, &id).await?;
    let me = cl::map_err(prov.as_ref());
    if suspend {
        prov.suspend(&cred, &id).await.map_err(&me)?;
    } else {
        prov.resume(&cred, &id).await.map_err(&me)?;
    }
    if let Ok(rb) = prov.get(&cred, &id).await {
        rec.state = rb.state.as_str().to_string();
        rec.url = rb.url.or(rec.url);
    }
    if !suspend {
        rec.last_activity_at = vk_cloud::now_s();
    }
    cl::save_record(server, &rec);
    Ok(view(server, &rec))
}

async fn box_checkpoint(server: &Arc<Server>, p: &Value) -> R {
    let box_ref = req(p, "box")?;
    let (pid, id) = parse_box_ref(box_ref)?;
    let (prov, cred) = credential(server, &pid)?;
    record_or_remote(server, prov.as_ref(), &cred, box_ref, &id).await?;
    let note = s(p, "note").unwrap_or("vibeke checkpoint");
    let checkpoint = prov
        .checkpoint(&cred, &id, note)
        .await
        .map_err(cl::map_err(prov.as_ref()))?;
    Ok(json!({"box": box_ref, "checkpoint": checkpoint}))
}

/// Close the panes of `rec`'s task that run in the box, and stop serving the task from it. A
/// task that is still active keeps its level and fails closed for new panes. Only for the box
/// the task runs in now (`current`, [`cl::is_current_box`]): destroying an older box of a task
/// that moved on to another box leaves the task's panes and context alone.
fn close_and_detach(server: &Arc<Server>, rec: &BoxRecord, current: bool) {
    if rec.key.is_empty() || !current {
        return;
    }
    let (panes, checkout, active) = server.with_core(|c| {
        let t = rec.task.as_deref().and_then(|t| c.task(t)).cloned();
        let ws = t.as_ref().and_then(|t| t.workspace.clone());
        let panes: Vec<String> = c
            .model
            .panes
            .iter()
            .filter(|p| {
                ws.as_deref() == Some(p.workspace.as_str())
                    && p.isolation.level == vk_proto::model::IsolationLevel::Cloud
            })
            .map(|p| p.id.clone())
            .collect();
        (
            panes,
            t.as_ref().and_then(|t| t.worktree_path.clone()),
            t.is_some_and(|t| t.status == "active"),
        )
    });
    for pane in panes {
        server.close_pane(&pane);
    }
    let was_live = cl::detach(server, &rec.key).is_some();
    if was_live && active {
        server.sandbox.mark_failed(
            &rec.key,
            checkout.as_deref().map(std::path::Path::new),
            format!("its cloud box {} was destroyed", rec.box_ref()),
        );
    }
}

/// Destroy the box of `rec` after the unsynced guard (unless `force`): its panes close first.
pub(crate) async fn destroy_record(
    server: &Arc<Server>,
    rec: &BoxRecord,
    force: bool,
) -> Result<(), RpcError> {
    let c = match server
        .sandbox
        .get(&rec.key)
        .as_deref()
        .and_then(cl::ctx)
        .filter(|c| c.box_ref() == rec.box_ref())
    {
        Some(c) => c.clone(),
        None => ctx_from_record(server, rec)?,
    };
    if !force {
        let u = cl::unsynced(server, &c).await?;
        if !u.is_clean() {
            return Err(cl::unsynced_conflict(&rec.box_ref(), &u));
        }
    }
    let current = cl::is_current_box(server, &rec.key, &rec.box_ref());
    close_and_detach(server, rec, current);
    cl::destroy_box_with(server, &c, current).await
}

async fn box_destroy(server: &Arc<Server>, p: &Value) -> R {
    let box_ref = req(p, "box")?;
    let (pid, id) = parse_box_ref(box_ref)?;
    let (prov, cred) = credential(server, &pid)?;
    let rec = record_or_remote(server, prov.as_ref(), &cred, box_ref, &id).await?;
    let force = b(p, "force").unwrap_or(false);
    destroy_record(server, &rec, force).await?;
    Ok(json!({"box": box_ref, "destroyed": true}))
}

async fn box_adopt(server: &Arc<Server>, p: &Value) -> R {
    let box_ref = req(p, "box")?;
    let (pid, id) = parse_box_ref(box_ref)?;
    let (prov, cred) = credential(server, &pid)?;
    record_or_remote(server, prov.as_ref(), &cred, box_ref, &id).await?;
    // TODO(spec 17 §6.3): create a host task with a worktree on the box's branch (pulled from
    // the box), then open a pane per live session.
    Err(err(
        ErrorKind::Unsupported,
        "adopting a cloud box is not available yet; destroy it with cloud.box.destroy or keep it",
    ))
}

fn box_forget(server: &Arc<Server>, p: &Value) -> R {
    let box_ref = req(p, "box")?;
    let (pid, _) = parse_box_ref(box_ref)?;
    credential(server, &pid)?;
    let rec = load_record(server, box_ref).ok_or_else(|| not_found("cloud box", box_ref))?;
    if rec.ownership != "missing" {
        return Err(err(
            ErrorKind::Conflict,
            format!(
                "{box_ref} is {}, not missing; only a box the provider no longer lists can be forgotten",
                rec.ownership
            ),
        )
        .details(json!({"ownership": rec.ownership})));
    }
    cl::drop_record(server, &rec);
    Ok(json!({"box": box_ref}))
}

/// Providers with a resolvable credential.
fn signed_in() -> Vec<Arc<dyn Provider>> {
    let cfg = cl::cloud_cfg();
    let Ok(kc) = keychain() else {
        return vec![];
    };
    vk_cloud::providers(&cfg)
        .into_iter()
        .filter(|p| {
            matches!(
                vk_cloud::auth::resolve(p.as_ref(), &cfg, &kc, &cl::host_env),
                Ok(Some(_))
            )
        })
        .collect()
}

async fn prune(server: &Arc<Server>, p: &Value) -> R {
    let provider = s(p, "provider");
    let owns: Vec<String> = match p.get("ownership") {
        None | Some(Value::Null) => vec!["orphaned".into(), "idle".into()],
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .filter(|o| PRUNABLE.contains(o))
                    .map(str::to_string)
                    .ok_or_else(|| invalid("ownership values are orphaned | idle | missing"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(invalid("ownership is a list")),
    };
    let dry_run = b(p, "dry_run").unwrap_or(false);
    let force = b(p, "force").unwrap_or(false);
    match provider {
        Some(pid) => {
            credential(server, pid)?;
        }
        None if signed_in().is_empty() => {
            // Nothing to prune without a sign-in: say which provider to sign in to.
            credential(server, &cl::cloud_cfg().default_provider)?;
        }
        None => {}
    }
    crate::cloud_reconcile::run(server, provider).await;
    let candidates: Vec<BoxRecord> = list_records(server)
        .into_iter()
        .filter(|r| provider.is_none_or(|x| r.provider == x))
        .filter(|r| owns.contains(&r.ownership))
        .collect();
    let views: Vec<Value> = candidates.iter().map(|r| view(server, r)).collect();
    let mut destroyed = Vec::new();
    let mut skipped = Vec::new();
    if !dry_run {
        for r in &candidates {
            if r.ownership == "missing" {
                // Gone at the provider already: only the record goes.
                cl::drop_record(server, r);
                destroyed.push(json!(r.box_ref()));
                continue;
            }
            match destroy_record(server, r, force).await {
                Ok(()) => destroyed.push(json!(r.box_ref())),
                Err(e) => {
                    let reason = e.data.details["reason"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or(e.message);
                    skipped.push(json!({"box": r.box_ref(), "reason": reason}));
                }
            }
        }
    }
    Ok(json!({"candidates": views, "destroyed": destroyed, "skipped": skipped}))
}
