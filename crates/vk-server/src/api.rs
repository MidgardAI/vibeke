//! JSON-RPC 2.0 control API (07 §1–2). One request per line; responses may be out of order.

use crate::core::{Tx, subject_pane};
use crate::{Server, render};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vk_proto::layout::{self, Direction};
use vk_proto::model::*;
use vk_proto::rpc::{ErrorKind, Request, Response, RpcError};

pub type R = Result<Value, RpcError>;

/// Caller identity for one connection (09 §3.2).
#[derive(Debug, Clone)]
pub struct Ctx {
    pub client_id: String,
    pub kind: String,
    /// Pane whose token authenticated this connection (pane scope), if any.
    pub pane_scope: Option<String>,
    pub remote: bool,
}

pub fn err(kind: ErrorKind, msg: impl Into<String>) -> RpcError {
    RpcError::new(kind, msg)
}

pub fn not_found(what: &str, t: &str) -> RpcError {
    err(ErrorKind::NotFound, format!("{what} not found: {t}"))
        .details(json!({"object": what, "target": t}))
}

pub fn invalid(msg: impl Into<String>) -> RpcError {
    err(ErrorKind::InvalidParams, msg)
}

pub fn internal(e: impl std::fmt::Display) -> RpcError {
    let s = e.to_string();
    if s.contains("storage") || s.contains("database") || s.contains("disk") {
        return err(ErrorKind::StorageUnavailable, s);
    }
    err(ErrorKind::Internal, s)
}

pub fn s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str)
}
pub fn b(p: &Value, k: &str) -> Option<bool> {
    p.get(k).and_then(Value::as_bool)
}
pub fn u(p: &Value, k: &str) -> Option<u64> {
    p.get(k).and_then(Value::as_u64)
}
pub fn req<'a>(p: &'a Value, k: &str) -> Result<&'a str, RpcError> {
    s(p, k).ok_or_else(|| invalid(format!("missing param `{k}`")))
}
pub fn argv(p: &Value, k: &str) -> Option<Vec<String>> {
    match p.get(k) {
        Some(Value::Array(a)) => Some(
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        ),
        Some(Value::String(s)) if !s.is_empty() => {
            Some(vec!["/bin/sh".into(), "-c".into(), s.clone()])
        }
        _ => None,
    }
}

pub fn cursor(server: &Server, seq: Option<i64>) -> Value {
    server.with_core(|c| {
        let seq = seq.unwrap_or_else(|| c.store.last_seq().unwrap_or(0));
        serde_json::to_value(c.store.cursor(seq)).unwrap_or(Value::Null)
    })
}

/// Resolve a pane target (07 §1.2). With a pane token and no target, `@current`.
pub fn resolve_pane(server: &Server, ctx: &Ctx, target: Option<&str>) -> Result<Pane, RpcError> {
    let t = match target {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => match &ctx.pane_scope {
            Some(p) => p.clone(),
            None => return Err(invalid("pane target required (no pane token for @current)")),
        },
    };
    let id = match t.as_str() {
        "@current" => ctx
            .pane_scope
            .clone()
            .ok_or_else(|| invalid("@current needs a pane token (VIBEKE_PANE_TOKEN)"))?,
        "@focused" => server
            .client_focus(&ctx.client_id)
            .pane
            .or_else(|| server.focused_pane())
            .ok_or_else(|| not_found("pane", "@focused"))?,
        other => other.to_string(),
    };
    server.with_core(|c| {
        c.pane(&id)
            .cloned()
            .or_else(|| c.run(&id).and_then(|r| c.pane(&r.pane).cloned()))
            .ok_or_else(|| not_found("pane", &id))
    })
}

pub fn resolve_ws(server: &Server, ctx: &Ctx, target: Option<&str>) -> Result<Workspace, RpcError> {
    let t = match target {
        Some(t) => t.to_string(),
        None => {
            let p = resolve_pane(server, ctx, None).ok();
            match p {
                Some(p) => p.workspace,
                None => server
                    .client_focus(&ctx.client_id)
                    .workspace
                    .ok_or_else(|| invalid("workspace target required"))?,
            }
        }
    };
    server
        .with_core(|c| {
            c.ws(&t).cloned().or_else(|| {
                c.model
                    .workspaces
                    .iter()
                    .find(|w| w.display_name() == t)
                    .cloned()
            })
        })
        .ok_or_else(|| not_found("workspace", &t))
}

pub fn resolve_tab(server: &Server, ctx: &Ctx, target: Option<&str>) -> Result<Tab, RpcError> {
    match target {
        Some(t) => server
            .with_core(|c| c.tab(t).cloned())
            .ok_or_else(|| not_found("tab", t)),
        None => {
            let p = resolve_pane(server, ctx, None)
                .ok()
                .map(|p| p.tab)
                .or_else(|| server.client_focus(&ctx.client_id).tab);
            let t = p.ok_or_else(|| invalid("tab target required"))?;
            server
                .with_core(|c| c.tab(&t).cloned())
                .ok_or_else(|| not_found("tab", &t))
        }
    }
}

/// Handle one JSON-RPC line; returns the response line (without newline). Notifications
/// (no id) still return a response string, which the caller may drop.
pub async fn handle_line(server: &Arc<Server>, ctx: &Ctx, line: &str) -> String {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            return serde_json::to_string(&Response::err(
                Value::Null,
                err(ErrorKind::ParseError, e.to_string()),
            ))
            .unwrap();
        }
    };
    let id = req.id.clone().unwrap_or(Value::Null);
    let resp = match dispatch(server, ctx, &req.method, &req.params).await {
        Ok(v) => Response::ok(id, v),
        Err(e) => Response::err(id, e),
    };
    serde_json::to_string(&resp).unwrap()
}

pub const METHODS: &[(&str, bool)] = &[
    ("client.hello", false),
    ("client.list", false),
    ("api.methods", false),
    ("api.schema", false),
    ("server.status", false),
    ("server.stop", true),
    ("server.reload_config", true),
    ("session.snapshot", false),
    ("workspace.list", false),
    ("workspace.get", false),
    ("workspace.create", true),
    ("workspace.rename", true),
    ("workspace.focus", true),
    ("workspace.close", true),
    ("workspace.move", true),
    ("tab.list", false),
    ("tab.create", true),
    ("tab.rename", true),
    ("tab.focus", true),
    ("tab.close", true),
    ("tab.move", true),
    ("pane.list", false),
    ("pane.get", false),
    ("pane.current", false),
    ("pane.split", true),
    ("pane.focus", true),
    ("pane.close", true),
    ("pane.zoom", true),
    ("pane.resize", true),
    ("pane.equalize", true),
    ("pane.rename", true),
    ("pane.send_text", true),
    ("pane.send_keys", true),
    ("pane.send_bytes", true),
    ("pane.run", true),
    ("pane.read", false),
    ("pane.wait_output", false),
    ("pane.wait_idle", false),
    ("pane.mark_unread", true),
    ("pane.mark_seen", true),
    ("pane.pin", true),
    ("pane.can_see_paths", false),
    ("notification.list", false),
    ("notification.send", true),
    ("notification.read", true),
    ("events.subscribe", false),
    ("events.unsubscribe", false),
    ("events.read", false),
    ("events.wait", false),
    ("search.query", false),
    ("blob.put", true),
    ("blob.begin", true),
    ("blob.append", true),
    ("blob.commit", true),
    ("blob.abort", true),
    ("paste.translated", true),
    ("image.upload", true),
    ("git.status", false),
    ("git.diff", false),
    ("git.log", false),
    ("fs.list", false),
    ("fs.read", false),
    ("layout.export", false),
    ("task.create", true),
    ("task.list", false),
    ("task.get", false),
    ("task.finish", true),
    ("task.setup", true),
    ("task.pr", false),
    ("task.reconcile", true),
    ("task.review.candidates", false),
    ("task.review.get", false),
    ("task.review.diff", false),
    ("task.review.accept", true),
    ("task.check.list", false),
    ("task.check.authorize", true),
    ("task.check.run", true),
    ("task.check.cancel", true),
    ("task.check.get", false),
    // 15 T4 (crate::review::t4).
    ("task.review.snapshot", true),
    ("task.review.snapshot.gc", true),
    ("task.review.request_reviewer", true),
    ("task.review.start_reviewer", true),
    ("task.review.notes", false),
    ("task.review.note.classify", true),
    ("task.dependency.add", true),
    ("task.dependency.remove", true),
    ("task.dependency.list", false),
    ("task.effort.estimate", false),
    ("attention.list", false),
    ("attention.update", true),
    ("worktree.list", false),
    ("worktree.remove", true),
    ("worktree.repo_root", false),
    ("worktree.create", true),
    ("worktree.open", true),
];

/// Methods a pane-scoped caller may never call (09 §5.2): the explicit full-scope list, read
/// through [`pane_scope_of`] by both [`authorize`] and the API catalog's scope column
/// (`docs/api/methods.json`).
pub const PANE_FORBIDDEN: &[&str] = &[
    "interaction.answer",
    "interaction.cancel",
    "server.stop",
    "server.reload_config",
    "server.restart",
    "session.create",
    "session.stop",
    "session.rename",
    "config.set",
    "config.reload",
    "task.park",
    "task.resume",
    // Synchronized input and task lifecycle (adopt/archive/recreate/forget, port re-lease)
    // are user actions.
    "pane.sync_input",
    "tab.renumber",
    "task.archive",
    "task.adopt",
    "task.recreate",
    "task.forget",
    "task.ports.re_lease",
    "workspace.close",
    "workspace.rename",
    "workspace.move",
    "workspace.focus",
    "tab.close",
    "tab.rename",
    "tab.move",
    "tab.focus",
    "pane.focus",
    "task.finish",
    "task.setup",
    "worktree.remove",
    "render.attach",
    "policy.trust",
    // 2F: the approval history behind `policy.suggest`, and the manifest channel (network fetch,
    // pins), are the user's.
    "policy.suggest",
    "agent.manifests_check",
    "agent.manifest_pin",
    // 15 §11: agents can report observations but not confirm intent, bind, send, accept,
    // authorize verification or change priorities.
    "task.track",
    "task.intent.update",
    "task.bind",
    "task.unbind",
    "task.set",
    "task.message.prepare",
    "task.message.send",
    "task.message.cancel",
    "task.review.accept",
    "task.check.authorize",
    "task.check.run",
    "task.check.cancel",
    // 15 T4: snapshots, reviewer runs, finding classification and dependency links are
    // human decisions.
    "task.review.snapshot",
    "task.review.snapshot.gc",
    "task.review.request_reviewer",
    "task.review.start_reviewer",
    "task.review.note.classify",
    "task.dependency.add",
    "task.dependency.remove",
    "attention.update",
    "integration.install",
    "integration.uninstall",
    // Research R2/R3: agents can keep drafts/notes in their own workspace and search its
    // desk, but sending, resuming, focusing and purging are user actions.
    "draft.send",
    "draft.reconcile",
    "desk.open",
    "desk.resume",
    "desk.forget",
    "scrollback.forget",
    "desk.index",
    "desk.status",
    // Refused by their handlers for every pane-scoped call regardless of params; listed here
    // so `authorize` refuses them first and the catalog cannot call them pane-accessible
    // (the handler checks stay as defense in depth).
    "preview.mirror",
    "preview.unmirror",
    // 06 A11: only the user's client reports translated pastes.
    "paste.translated",
    "preview.profile.reset",
    "preview.profile_reset",
    "client.focus",
    "screenshot.delete",
    "browser.watch",
    "browser.install",
    "browser.take_over",
    "browser.release",
    "browser.attach_screencast",
    "browser.detach_screencast",
    "browser.screencast_frame",
    "browser.pane.update",
    "browser.command",
    "browser.pane.console",
    "browser.pane.console_push",
    "group.create",
    "group.rename",
    "group.move",
    "group.delete",
    "group.collapse",
    "group.add",
    "group.remove",
    "sandbox.start",
    "sandbox.stop",
    "sandbox.remove",
    "sandbox.allow",
    // 13 §8/§11: user-side box control; a contained run asks with `sandbox.request`.
    "sandbox.disallow",
    "sandbox.shell",
    "sandbox.logs",
    "sandbox.prune",
    "sandbox.recover",
    "sandbox.relaunch",
    "sandbox.push",
    "sandbox.copy_out",
    "sandbox.setup_token",
    "task.sync",
    "compat.invocation.verify",
    "plugin.surface.close",
    "plugin.registry.notify",
];

/// Method prefixes whose every method is forbidden for pane scope (14 §9: pane/adapter tokens
/// get no assistant access).
pub const PANE_FORBIDDEN_PREFIXES: &[&str] = &["assistant."];

// Handlers that refuse pane scope only for some params stay `Open`/`OwnTarget` here (their
// handler checks are authoritative), e.g. `preview.profile {action: "reset"}`, `preview.open`
// of a non-loopback URL, `browser.pane.create` of a non-loopback URL or next to another
// pane, `browser.eval` without the `browser.script` capability (`preview.browser_script`),
// `git.status|diff {path}`, `worktree.create {focus: true}`, `compat.herdr.call` of a
// focus/close method, `screenshot.*` / `task.*` / `draft.*` reads outside the caller's
// workspace, and `task.operation.get` receipts.

/// How a pane-scoped caller may use a method (09 §5.2). The single source for both
/// [`authorize`] (dispatch) and the generated API catalog (`docs/api/methods.json`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneScope {
    /// Full scope only: a pane token is refused.
    Forbidden,
    /// Callable from a pane, but only against the caller's own panes / runs.
    OwnTarget,
    /// Callable from a pane (handlers may still refuse particular params).
    Open,
}

impl PaneScope {
    pub fn as_str(self) -> &'static str {
        match self {
            PaneScope::Forbidden => "forbidden",
            PaneScope::OwnTarget => "own_target",
            PaneScope::Open => "open",
        }
    }
}

/// The pane scope of `method` (see [`PaneScope`]).
pub fn pane_scope_of(method: &str) -> PaneScope {
    if PANE_FORBIDDEN.contains(&method)
        || crate::security::PANE_FORBIDDEN.contains(&method)
        || crate::plugin_native::PANE_FORBIDDEN.contains(&method)
        || crate::privacy::PANE_FORBIDDEN.contains(&method)
        || crate::orch::PANE_FORBIDDEN.contains(&method)
        || crate::blob_store::PANE_FORBIDDEN.contains(&method)
        || crate::browse_api::PANE_FORBIDDEN.contains(&method)
        || crate::hardening::PANE_FORBIDDEN.contains(&method)
        || crate::machines::PANE_FORBIDDEN.contains(&method)
        || crate::items::PANE_FORBIDDEN.contains(&method)
        || crate::review::pr::PANE_FORBIDDEN.contains(&method)
        || crate::collision::PANE_FORBIDDEN.contains(&method)
        || crate::review::ext::PANE_FORBIDDEN.contains(&method)
        || PANE_FORBIDDEN_PREFIXES
            .iter()
            .any(|p| method.starts_with(p))
    {
        PaneScope::Forbidden
    } else if is_pane_targeted(method)
        || is_run_targeted(method)
        || matches!(method, "agent.start" | "agent.resume")
    {
        PaneScope::OwnTarget
    } else {
        PaneScope::Open
    }
}

/// `pane.*` methods that act on a pane (a pane-scoped caller may only target its own panes).
pub fn is_pane_targeted(method: &str) -> bool {
    method.starts_with("pane.")
        && !matches!(
            method,
            "pane.list"
                | "pane.get"
                | "pane.current"
                | "pane.read"
                | "pane.wait_output"
                | "pane.wait_idle"
                | "pane.can_see_paths"
        )
}

/// `agent.*` methods whose `target` must be the caller's own run or a pane it created.
pub fn is_run_targeted(method: &str) -> bool {
    matches!(
        method,
        "agent.prompt" | "agent.interrupt" | "agent.send_keys" | "agent.rename" | "agent.release"
    )
}

/// Capability check for pane-scoped callers (09 §5.2): reads are open; writes are limited to
/// the caller's own pane and panes it created; authorizing actions (answering interactions),
/// server control and other workspaces' layout are forbidden.
pub fn authorize(server: &Server, ctx: &Ctx, method: &str, p: &Value) -> Result<(), RpcError> {
    crate::plugin_native::authorize(server, ctx, method, p)?;
    let Some(scope) = &ctx.pane_scope else {
        return Ok(());
    };
    let deny = |why: &str| {
        Err(err(
            ErrorKind::PermissionDenied,
            format!("{method} is not allowed from a pane ({why})"),
        )
        .details(json!({"scope": "pane"})))
    };
    if pane_scope_of(method) == PaneScope::Forbidden {
        if method == "interaction.answer" {
            return Err(err(
                ErrorKind::PermissionDenied,
                "self_answer_forbidden: agents may see but not answer interactions",
            )
            .details(json!({"scope": "pane"})));
        }
        return deny("forbidden for pane scope");
    }
    crate::preview::authorize_pane_machine(server, ctx, method, p)?;
    let owns = |pane: &Pane| &pane.id == scope || pane.created_by == format!("agent:{scope}");
    let pane_targeted = is_pane_targeted(method);
    if pane_targeted {
        let target = s(p, "pane").unwrap_or("@current");
        let pane = resolve_pane(server, ctx, Some(target))?;
        if !owns(&pane) {
            return deny("target pane is not yours");
        }
        if p.get("focus").and_then(Value::as_bool) == Some(true) {
            return deny("agents can't move the user's focus");
        }
        // 09 §5.1 rule 4: no pane-scoped input into a pane with an open interaction.
        let writes_input = matches!(
            method,
            "pane.send_text" | "pane.send_keys" | "pane.send_bytes" | "pane.run"
        );
        if writes_input
            && server.with_core(|c| {
                c.model
                    .interactions
                    .iter()
                    .any(|i| i.pane == pane.id && i.status == InteractionStatus::Open)
            })
        {
            return Err(err(
                ErrorKind::PermissionDenied,
                "input_locked_open_interaction: the pane has an open interaction",
            )
            .details(json!({"scope": "pane"})));
        }
    }
    let run_targeted = is_run_targeted(method);
    if run_targeted {
        let t = s(p, "target").unwrap_or("@current");
        let pane = server
            .with_core(|c| c.run(t).and_then(|r| c.pane(&r.pane).cloned()))
            .map(Ok)
            .unwrap_or_else(|| resolve_pane(server, ctx, Some(t)))?;
        if !owns(&pane) {
            return deny("target agent is not in your pane or a pane you created");
        }
    }
    if matches!(method, "agent.start" | "agent.resume") {
        let pane = resolve_pane(server, ctx, s(p, "pane").or(Some("@current")))?;
        if !owns(&pane) {
            return deny("target pane is not yours");
        }
    }
    Ok(())
}

pub async fn dispatch(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> R {
    authorize(server, ctx, method, p)
        .inspect_err(|e| crate::security::denied(server, ctx, method, e))?;
    crate::security::authorize(server, ctx, method)?;
    crate::search::authorize_read(server, ctx, method, p)?;
    crate::browser_pane::page_io::authorize_output_read(server, ctx, method, p)?;
    crate::limits::check(server, ctx, method, p)?;
    // Lane 3E: search result redaction, state.forget, encryption status/migrate.
    if let Some(r) = crate::privacy::api(server, ctx, method, p).await {
        return r;
    }
    // Batch 4 orchestration (best-of-N, split, learned policy, merge, goals, quota, vm).
    if let Some(r) = crate::orch::api(server, ctx, method, p).await {
        return r;
    }
    // Batch 2A API surface: one hook per module.
    if let Some(r) = crate::config_api::api(server, method, p).await {
        return r;
    }
    if let Some(r) = crate::session_api::api(server, ctx, method, p).await {
        return r;
    }
    if let Some(r) = crate::blob_api::api(server, ctx, method, p) {
        return r;
    }
    if let Some(r) = crate::blob_store::api(server, method, p) {
        return r;
    }
    if let Some(r) = crate::hardening::api(server, method, p) {
        return r;
    }
    if let Some(r) = crate::machines::api(server, ctx, method, p) {
        return r;
    }
    if let Some(r) = crate::items::api(server, method, p) {
        return r;
    }
    if let Some(r) = crate::collision::api(server, ctx, method, p).await {
        return r;
    }
    if let Some(r) = crate::pane_api::api(server, ctx, method, p) {
        return r;
    }
    if let Some(r) = crate::sync_input::api(server, ctx, method, p) {
        return r;
    }
    if let Some(r) = crate::tab_renumber::api(server, ctx, method, p) {
        return r;
    }
    if let Some(r) = Box::pin(crate::task_lifecycle::api(server, ctx, method, p)).await {
        return r;
    }
    if let Some(r) = Box::pin(crate::task_park::api(server, ctx, method, p)).await {
        return r;
    }
    if let Some(r) = crate::security::api(server, ctx, method, p).await {
        return r;
    }
    if let Some(r) = crate::parity::api(server, ctx, method, p).await {
        return r;
    }
    if let Some(r) = crate::gateway_api::api(server, ctx, method, p).await {
        return r;
    }
    if let Some(r) = crate::assist::api(server, ctx, method, p).await {
        return r;
    }
    if let Some(r) = crate::agents::api(server, ctx, method, p).await {
        return r;
    }
    if let Some(r) = crate::run::tasks_api(server, ctx, method, p).await {
        return r;
    }
    if method.starts_with("attention.")
        && let Some(r) = crate::review::attention_api(server, ctx, method, p).await
    {
        return r;
    }
    if let Some(r) = crate::preview::api(server, ctx, method, p).await {
        return r;
    }
    if let Some(r) = crate::screenshots::api(server, ctx, method, p).await {
        return r;
    }
    if let Some(r) = crate::agent_browser::api(server, ctx, method, p).await {
        return r;
    }
    // Boxed: these futures are large and would overflow a worker stack in debug builds.
    if let Some(r) = Box::pin(crate::git_api::api(server, ctx, method, p)).await {
        return r;
    }
    // Boxed: these futures are large and would overflow a worker stack in debug builds.
    if let Some(r) = Box::pin(crate::fs_api::api(server, ctx, method, p)).await {
        return r;
    }
    if let Some(r) = crate::browse_api::api(server, method, p).await {
        return r;
    }
    if let Some(r) = crate::desk::api(server, ctx, method, p).await {
        return r;
    }
    if let Some(r) = crate::drafts::api(server, ctx, method, p).await {
        return r;
    }
    // Native plugins (lane 3B): its own methods and the merged shared `plugin.*` views.
    if (method.starts_with("plugin.") || method.starts_with("ui.") || method == "compat.ui.state")
        && let Some(r) = Box::pin(crate::plugin_native::api(server, ctx, method, p)).await
    {
        return r;
    }
    if (method.starts_with("plugin.") || method.starts_with("compat."))
        && let Some(r) = crate::compat::api(server, ctx, method, p).await
    {
        return r;
    }
    match method {
        "client.hello" => Ok(json!({
            "server_version": vk_proto::VERSION,
            "api": vk_proto::API_VERSION,
            "session": server.opts.session,
            "machine": server.opts.machine,
            "capabilities": if ctx.pane_scope.is_some() { json!(["pane"]) } else { json!(["*"]) },
            "features": ["render.v1", "events.v1"],
            "client_id": ctx.client_id,
        })),
        "client.list" => {
            let clients = server.clients.lock().unwrap();
            Ok(
                json!({"clients": clients.iter().map(|(id, c)| json!({"id": id, "kind": c.kind, "attached_at": c.attached_at_ms, "focused_pane": c.focus.pane})).collect::<Vec<_>>()}),
            )
        }
        "api.methods" => {
            // One source of truth: the same tables that drive shapes, scopes and read-only gates.
            let v: Vec<Value> = crate::api_schema::method_tables()
                .iter()
                .flat_map(|(_, t)| t.iter())
                .map(|(n, m)| json!({"name": n, "mutating": m}))
                .collect();
            Ok(json!({"methods": v}))
        }
        "api.schema" => {
            let method = s(p, "method");
            match crate::api_schema::api_schema(method) {
                Some(schema) => Ok(json!({"schema": schema})),
                None => Err(err(
                    ErrorKind::NotFound,
                    format!("no schema for method {}", method.unwrap_or_default()),
                )
                .details(json!({"object": "method"}))),
            }
        }
        "server.status" => {
            let (panes, seq) =
                server.with_core(|c| (c.model.panes.len(), c.store.last_seq().unwrap_or(0)));
            Ok(json!({
                "pid": std::process::id(),
                "version": vk_proto::VERSION,
                "uptime_ms": server.started.elapsed().as_millis() as u64,
                "boot_id": server.boot_id,
                "restart_error": *server.restart_error.lock().unwrap(),
                "session": server.opts.session,
                "machine": server.opts.machine,
                "panes": panes,
                "holders": {"live": server.panes.lock().unwrap().len()},
                "clients": server.clients.lock().unwrap().len(),
                "event_seq": seq,
                "socket": server.paths.socket(),
                "degraded": *server.degraded.lock().unwrap(),
                "ephemeral": server.hardening.ephemeral(),
                "preview": crate::preview::status_json(server),
                "timers": crate::timers::status_json(server),
            }))
        }
        "server.stop" => {
            if ctx.pane_scope.is_some() {
                return Err(err(
                    ErrorKind::PermissionDenied,
                    "server.stop is not allowed from a pane token",
                ));
            }
            let kill = b(p, "kill_panes").unwrap_or(false);
            if kill {
                for rt in server.panes.lock().unwrap().values() {
                    rt.send(crate::pane::PaneCmd::Close);
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            // Snapshot every pane so the next server replays as little as possible.
            for rt in server.panes.lock().unwrap().values() {
                rt.send(crate::pane::PaneCmd::Snapshot);
            }
            let srv = server.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                srv.housekeeping();
                crate::machines::stopped(&srv, "api");
                srv.shutdown.notify_waiters();
                let _ = srv
                    .ui
                    .send(crate::UiEvent::Goodbye("server stopped".into()));
                tokio::time::sleep(Duration::from_millis(100)).await;
                std::process::exit(0);
            });
            Ok(json!({}))
        }
        "session.snapshot" => Ok(server.snapshot_json()),

        // ---- workspaces -----------------------------------------------------------------
        "workspace.list" => {
            let c = server.core.lock().unwrap();
            let list: Vec<Value> = c
                .model
                .workspaces
                .iter()
                .map(|w| {
                    let runs: Vec<&AgentRun> = c.model.runs.iter().filter(|r| c.pane(&r.pane).is_some_and(|p| p.workspace == w.id)).collect();
                    let mut v = serde_json::to_value(w).unwrap();
                    v["name"] = json!(w.display_name());
                    v["tab_count"] = json!(c.tabs_of(&w.id).len());
                    v["pane_count"] = json!(c.model.panes.iter().filter(|p| p.workspace == w.id).count());
                    v["agent_summary"] = json!({
                        "working": runs.iter().filter(|r| r.execution.value == Execution::Working).count(),
                        "idle": runs.iter().filter(|r| r.execution.value == Execution::Idle).count(),
                        "needs_input": c.model.interactions.iter().filter(|i| runs.iter().any(|r| r.id == i.run) && i.status == InteractionStatus::Open).count(),
                    });
                    v
                })
                .collect();
            Ok(json!({"workspaces": list}))
        }
        "workspace.get" => Ok(json!({"workspace": resolve_ws(server, ctx, s(p, "workspace"))?})),
        "workspace.create" => {
            let cwd = s(p, "cwd")
                .map(str::to_string)
                .unwrap_or_else(|| crate::paths::home().to_string_lossy().into_owned());
            let focus = b(p, "focus")
                .unwrap_or(false)
                .then_some(ctx.client_id.as_str());
            let (ws, tab, pane) = server
                .create_workspace(
                    &cwd,
                    s(p, "name").map(Into::into),
                    argv(p, "command"),
                    focus,
                )
                .map_err(internal)?;
            Ok(
                json!({"workspace": ws, "tab": tab, "root_pane": pane, "cursor": cursor(server, None)}),
            )
        }
        "workspace.rename" => {
            let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
            let mut c = server.core.lock().unwrap();
            let mut w = ws.clone();
            w.name = s(p, "name").map(str::to_string).filter(|s| !s.is_empty());
            let mut tx = Tx::new();
            tx.event(
                "workspace.renamed",
                json!({"workspace": w.id}),
                json!({"name": w.name}),
            );
            tx.ws(w.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"workspace": w}))
        }
        "workspace.move" => {
            let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
            let delta = p.get("delta").and_then(Value::as_i64).unwrap_or(0);
            let mut c = server.core.lock().unwrap();
            let mut list = c.model.workspaces.clone();
            let i = list.iter().position(|w| w.id == ws.id).unwrap_or(0) as i64;
            let j = (i + delta).clamp(0, list.len() as i64 - 1) as usize;
            let w = list.remove(i as usize);
            list.insert(j, w);
            let mut tx = Tx::new();
            for (k, mut w) in list.into_iter().enumerate() {
                w.order = k as f64 + 1.0;
                tx.ws(w);
            }
            tx.event(
                "workspace.moved",
                json!({"workspace": ws.id}),
                json!({"index": j}),
            );
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({}))
        }
        "workspace.focus" => {
            let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
            let cur = server.client_focus(&ctx.client_id);
            let pane = server.with_core(|c| {
                let tabs = c.tabs_of(&ws.id);
                let tab = tabs
                    .iter()
                    .find(|t| Some(&t.id) == cur.tab.as_ref())
                    .or(tabs.first())
                    .map(|t| (*t).clone());
                tab.and_then(|t| t.focused_pane.or_else(|| t.layout.panes().first().cloned()))
            });
            if let Some(pane) = pane {
                server.focus_pane(&ctx.client_id, &pane);
            }
            Ok(json!({"workspace": ws}))
        }
        "workspace.close" => {
            let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
            let panes: Vec<String> = server.with_core(|c| {
                c.model
                    .panes
                    .iter()
                    .filter(|x| x.workspace == ws.id)
                    .map(|x| x.id.clone())
                    .collect()
            });
            for pid in panes {
                server.close_pane(&pid);
            }
            Ok(json!({}))
        }

        // ---- tabs -----------------------------------------------------------------------
        "tab.list" => {
            let ws = s(p, "workspace")
                .map(|w| resolve_ws(server, ctx, Some(w)))
                .transpose()?;
            let tabs: Vec<Tab> = server.with_core(|c| {
                c.model
                    .tabs
                    .iter()
                    .filter(|t| ws.as_ref().is_none_or(|w| w.id == t.workspace))
                    .cloned()
                    .collect()
            });
            Ok(json!({"tabs": tabs}))
        }
        "tab.create" => {
            let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
            let focus = b(p, "focus")
                .unwrap_or(false)
                .then_some(ctx.client_id.as_str());
            let cwd = s(p, "cwd").map(str::to_string).or_else(|| {
                resolve_pane(server, ctx, None)
                    .ok()
                    .and_then(|x| server.pane_cwd(&x.id))
            });
            let (tab, pane) = server
                .create_tab(
                    &ws.id,
                    cwd.as_deref(),
                    s(p, "title").map(Into::into),
                    argv(p, "command"),
                    focus,
                )
                .map_err(internal)?;
            Ok(json!({"tab": tab, "root_pane": pane, "cursor": cursor(server, None)}))
        }
        "tab.rename" => {
            let t = resolve_tab(server, ctx, s(p, "tab"))?;
            let mut c = server.core.lock().unwrap();
            let mut t = t.clone();
            t.title = s(p, "title").map(str::to_string).filter(|s| !s.is_empty());
            let mut tx = Tx::new();
            tx.event(
                "tab.renamed",
                json!({"tab": t.id}),
                json!({"title": t.title}),
            );
            tx.tab(t.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"tab": t}))
        }
        "tab.focus" => {
            let t = resolve_tab(server, ctx, s(p, "tab"))?;
            if let Some(pane) = t
                .focused_pane
                .clone()
                .or_else(|| t.layout.panes().first().cloned())
            {
                server.focus_pane(&ctx.client_id, &pane);
            }
            Ok(json!({"tab": t}))
        }
        "tab.close" => {
            let t = resolve_tab(server, ctx, s(p, "tab"))?;
            for pid in t.layout.panes() {
                server.close_pane(&pid);
            }
            Ok(json!({}))
        }
        "tab.move" => {
            let t = resolve_tab(server, ctx, s(p, "tab"))?;
            let delta = p.get("delta").and_then(Value::as_i64).unwrap_or(0);
            let mut c = server.core.lock().unwrap();
            let mut list: Vec<Tab> = c.tabs_of(&t.workspace).into_iter().cloned().collect();
            let i = list.iter().position(|x| x.id == t.id).unwrap_or(0) as i64;
            let j = (i + delta).clamp(0, list.len() as i64 - 1) as usize;
            let x = list.remove(i as usize);
            list.insert(j, x);
            let mut tx = Tx::new();
            for (k, mut x) in list.into_iter().enumerate() {
                x.order = k as f64 + 1.0;
                tx.tab(x);
            }
            tx.event(
                "tab.moved",
                json!({"tab": t.id, "workspace": t.workspace}),
                json!({"index": j}),
            );
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({}))
        }

        // ---- panes ----------------------------------------------------------------------
        "pane.list" => {
            let ws = s(p, "workspace")
                .map(|w| resolve_ws(server, ctx, Some(w)))
                .transpose()?;
            let tab = s(p, "tab")
                .map(|t| resolve_tab(server, ctx, Some(t)))
                .transpose()?;
            let panes: Vec<Value> = server.with_core(|c| {
                c.model
                    .panes
                    .iter()
                    .filter(|x| ws.as_ref().is_none_or(|w| w.id == x.workspace) && tab.as_ref().is_none_or(|t| t.id == x.tab))
                    .filter(|x| !b(p, "has_agent").unwrap_or(false) || c.run_for_pane(&x.id).is_some())
                    .map(|x| {
                        let mut v = serde_json::to_value(x).unwrap();
                        v["agent"] = json!(c.run_for_pane(&x.id).map(|r| json!({"handle": r.handle, "harness": r.harness, "name": r.name, "state": r.execution.value.as_str()})));
                        v
                    })
                    .collect()
            });
            Ok(json!({"panes": panes}))
        }
        "pane.get" | "pane.current" => {
            let pane = resolve_pane(
                server,
                ctx,
                if method == "pane.current" {
                    Some("@current")
                } else {
                    s(p, "pane")
                },
            )?;
            let (run, ints) = server.with_core(|c| {
                (
                    c.run_for_pane(&pane.id).cloned(),
                    c.model
                        .interactions
                        .iter()
                        .filter(|i| i.pane == pane.id && i.status == InteractionStatus::Open)
                        .cloned()
                        .collect::<Vec<_>>(),
                )
            });
            let rev = server.pane_rt(&pane.id).map(|r| r.rev());
            let history = server.pane_rt(&pane.id).map(|rt| {
                let sc = rt.screen.lock().unwrap();
                json!({"in_memory": sc.engine.history_len(), "scrolled_total": sc.engine.scrolled_total(), "archived_upto": sc.archived_upto})
            });
            let live = server.with_core(|c| c.model.live(&pane.id).cloned());
            Ok(
                json!({"pane": pane, "run": run, "open_interactions": ints, "revision": rev, "cwd": server.pane_cwd(&pane.id), "history": history, "live": live}),
            )
        }
        "pane.split" => {
            let target = resolve_pane(server, ctx, s(p, "pane"))?;
            let dir = Direction::parse(s(p, "direction").unwrap_or("right"))
                .ok_or_else(|| invalid("direction must be right|down|left|up"))?;
            let ratio = p.get("ratio").and_then(Value::as_f64).unwrap_or(0.5) as f32;
            let focus = b(p, "focus")
                .unwrap_or(false)
                .then_some(ctx.client_id.as_str());
            let by_owned = match &ctx.pane_scope {
                Some(p) => format!("agent:{p}"),
                None => "user".to_string(),
            };
            let by = by_owned.as_str();
            let pane = server
                .split_pane(
                    &target.id,
                    dir,
                    ratio,
                    s(p, "cwd"),
                    argv(p, "command"),
                    s(p, "title").map(Into::into),
                    focus,
                    by,
                )
                .map_err(internal)?;
            Ok(json!({"pane": pane, "cursor": cursor(server, None)}))
        }
        "pane.focus" => {
            let pane = match (s(p, "pane"), s(p, "direction")) {
                (_, Some(d)) => {
                    let cur = resolve_pane(server, ctx, s(p, "pane").or(Some("@focused")))?;
                    let dir = Direction::parse(d).ok_or_else(|| invalid("bad direction"))?;
                    let tab = server
                        .with_core(|c| c.tab(&cur.tab).cloned())
                        .ok_or_else(|| not_found("tab", &cur.tab))?;
                    let rects = layout::rects(
                        &tab.layout,
                        layout::Rect {
                            x: 0,
                            y: 0,
                            w: 400,
                            h: 200,
                        },
                    );
                    match layout::neighbor(&rects, &cur.id, dir) {
                        Some(n) => server
                            .with_core(|c| c.pane(&n).cloned())
                            .ok_or_else(|| not_found("pane", &n))?,
                        None => cur,
                    }
                }
                (t, None) => resolve_pane(server, ctx, t)?,
            };
            server.focus_pane(&ctx.client_id, &pane.id);
            Ok(json!({"pane": pane}))
        }
        "pane.close" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            server.close_pane(&pane.id);
            Ok(json!({}))
        }
        "pane.zoom" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let mut c = server.core.lock().unwrap();
            let mut tab = c
                .tab(&pane.tab)
                .cloned()
                .ok_or_else(|| not_found("tab", &pane.tab))?;
            let on = b(p, "zoomed").unwrap_or(tab.zoomed_pane.as_deref() != Some(&pane.id));
            tab.zoomed_pane = on.then(|| pane.id.clone());
            let mut tx = Tx::new();
            tx.tab(tab.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"tab": tab}))
        }
        "pane.resize" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let dir =
                Direction::parse(req(p, "direction")?).ok_or_else(|| invalid("bad direction"))?;
            let amount = p.get("percent").and_then(Value::as_f64).unwrap_or(5.0) as f32 / 100.0;
            let mut c = server.core.lock().unwrap();
            let mut tab = c
                .tab(&pane.tab)
                .cloned()
                .ok_or_else(|| not_found("tab", &pane.tab))?;
            layout::resize(&mut tab.layout, &pane.id, dir, amount);
            let mut tx = Tx::new();
            tx.tab(tab.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"layout": tab.layout}))
        }
        "pane.equalize" => {
            let tab = resolve_tab(server, ctx, s(p, "tab"))?;
            let mut c = server.core.lock().unwrap();
            let mut tab = tab.clone();
            layout::equalize(&mut tab.layout);
            let mut tx = Tx::new();
            tx.tab(tab.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"layout": tab.layout}))
        }
        "pane.rename" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let mut c = server.core.lock().unwrap();
            let mut x = pane.clone();
            x.title = s(p, "title").map(str::to_string).filter(|s| !s.is_empty());
            let mut tx = Tx::new();
            tx.event(
                "pane.title_changed",
                subject_pane(&x),
                json!({"title": x.title}),
            );
            tx.pane(x.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"pane": x}))
        }
        "pane.send_text" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let text = req(p, "text")?;
            let modes = render::input_modes(server, &pane.id);
            let bytes = match s(p, "paste").unwrap_or("auto") {
                "raw" => text.as_bytes().to_vec(),
                "bracketed" => {
                    let mut m = modes;
                    m.bracketed_paste = true;
                    vk_term::encode::encode_paste(text, &m)
                }
                _ => {
                    if modes.bracketed_paste {
                        vk_term::encode::encode_paste(text, &modes)
                    } else {
                        text.as_bytes().to_vec()
                    }
                }
            };
            let n = bytes.len();
            send(server, ctx, &pane.id, bytes).await?;
            Ok(json!({"bytes": n}))
        }
        "pane.send_bytes" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let data = req(p, "data_b64")?;
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|e| invalid(e.to_string()))?;
            send(server, ctx, &pane.id, bytes).await?;
            Ok(json!({}))
        }
        "pane.send_keys" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let keys = p
                .get("keys")
                .and_then(Value::as_array)
                .ok_or_else(|| invalid("keys must be an array"))?;
            let modes = render::input_modes(server, &pane.id);
            // Validate everything before writing a byte (07 §2.6.1).
            let mut bytes = Vec::new();
            for k in keys {
                let k = k.as_str().unwrap_or_default();
                let ev = vk_term::keygrammar::parse_key(k).map_err(|e| {
                    err(ErrorKind::InvalidKey, e.to_string()).details(json!({"key": k}))
                })?;
                bytes.extend(vk_term::encode::encode_key(&ev, &modes));
            }
            send(server, ctx, &pane.id, bytes).await?;
            Ok(json!({}))
        }
        "pane.run" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let cmd = req(p, "command")?;
            let rev0 = server.pane_rt(&pane.id).map(|r| r.rev()).unwrap_or(0);
            let mut bytes = cmd.as_bytes().to_vec();
            bytes.push(b'\r');
            send(server, ctx, &pane.id, bytes).await?;
            if b(p, "wait").unwrap_or(false) {
                let timeout = Duration::from_millis(u(p, "timeout_ms").unwrap_or(120_000));
                wait_idle(
                    server,
                    &pane.id,
                    Duration::from_millis(500),
                    timeout,
                    Some(rev0),
                )
                .await?;
                let text = read_text(server, &pane.id, "recent", 50);
                return Ok(json!({"output_tail": text}));
            }
            Ok(json!({}))
        }
        "pane.read" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let source = s(p, "source").unwrap_or("visible");
            let lines = u(p, "lines").unwrap_or(200) as usize;
            if matches!(source, "last-command" | "last_command") {
                return read_last_command(server, &pane.id, lines);
            }
            let text = read_text(server, &pane.id, source, lines);
            let rev = server.pane_rt(&pane.id).map(|r| r.rev()).unwrap_or(0);
            Ok(json!({"text": text, "revision": rev, "source": source}))
        }
        "pane.wait_output" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let pat = match (s(p, "regex"), s(p, "match")) {
                (Some(r), _) => regex::Regex::new(r).map_err(|e| invalid(e.to_string()))?,
                (None, Some(m)) => regex::Regex::new(&regex::escape(m)).unwrap(),
                _ => return Err(invalid("match or regex required")),
            };
            let timeout = Duration::from_millis(u(p, "timeout_ms").unwrap_or(30_000));
            let since = u(p, "since_revision");
            let deadline = Instant::now() + timeout;
            let rt = server
                .pane_rt(&pane.id)
                .ok_or_else(|| not_found("pane", &pane.id))?;
            let mut rx = rt.rev_tx.subscribe();
            loop {
                let rev = rt.rev();
                if since.is_none_or(|s| rev > s) {
                    let text = read_text(server, &pane.id, "recent", 500);
                    if let Some(m) = pat.find(&text) {
                        let line = text[..m.start()].matches('\n').count();
                        return Ok(json!({"matched": m.as_str(), "line": line, "revision": rev}));
                    }
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() || tokio::time::timeout(left, rx.changed()).await.is_err() {
                    return Err(err(ErrorKind::Timeout, "pattern not seen")
                        .details(json!({"revision": rt.rev()})));
                }
            }
        }
        "pane.wait_idle" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let quiet = Duration::from_millis(u(p, "quiet_ms").unwrap_or(2000));
            let timeout = Duration::from_millis(u(p, "timeout_ms").unwrap_or(60_000));
            let rev = wait_idle(server, &pane.id, quiet, timeout, None).await?;
            Ok(json!({"revision": rev}))
        }
        "pane.mark_unread" | "pane.mark_seen" | "pane.pin" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let mut c = server.core.lock().unwrap();
            let mut x = pane.clone();
            let mut tx = Tx::new();
            match method {
                "pane.mark_unread" => {
                    x.marked_unread = b(p, "marked").unwrap_or(!x.marked_unread);
                    tx.event(
                        "pane.marked_unread",
                        subject_pane(&x),
                        json!({"marked": x.marked_unread}),
                    );
                }
                "pane.mark_seen" => {
                    x.unread = false;
                    x.marked_unread = false;
                    if let Some(r) = c.run_for_pane(&x.id) {
                        tx.m.read_mark("local", &x.id, r.done_rev);
                    }
                    tx.event("pane.seen", subject_pane(&x), json!({}));
                }
                _ => {
                    x.pinned = b(p, "pinned").unwrap_or(!x.pinned);
                    tx.event("pane.pinned", subject_pane(&x), json!({"pinned": x.pinned}));
                }
            }
            tx.pane(x.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"pane": x}))
        }
        "pane.can_see_paths" => {
            // Host panes on this machine see every path; remote callers ask the remote server,
            // which can't see the client's files (06 A11.1).
            let paths = p
                .get("paths")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // A sandboxed pane sees only its allowlist (13 §5, 06 A11.4).
            let pane = s(p, "pane").and_then(|t| resolve_pane(server, ctx, Some(t)).ok());
            let visible: Vec<bool> = paths
                .iter()
                .map(|x| {
                    x.as_str().is_some_and(|path| {
                        std::path::Path::new(path).exists()
                            && pane
                                .as_ref()
                                .and_then(|pn| crate::sandbox::can_see(server, &pn.id, path))
                                .unwrap_or(true)
                    })
                })
                .collect();
            Ok(json!({"visible": visible}))
        }

        // ---- notifications ------------------------------------------------------------
        "notification.list" => {
            let unread = b(p, "unread_only").unwrap_or(false);
            let list: Vec<Notification> = server.with_core(|c| {
                c.notifications
                    .iter()
                    .rev()
                    .filter(|n| !unread || !n.read)
                    .take(u(p, "limit").unwrap_or(50) as usize)
                    .cloned()
                    .collect()
            });
            Ok(json!({"notifications": list}))
        }
        "notification.send" => {
            let pane = resolve_pane(server, ctx, s(p, "pane")).ok().map(|x| x.id);
            let n = server.notify(
                "plugin",
                pane.as_deref(),
                req(p, "title")?,
                s(p, "body").unwrap_or(""),
                s(p, "urgency").unwrap_or("normal"),
            );
            Ok(json!({"notification": n}))
        }
        "notification.read" => {
            server.with_core(|c| {
                let all = b(p, "all").unwrap_or(false);
                let id = s(p, "notification");
                for n in c.notifications.iter_mut() {
                    if all || Some(n.id.as_str()) == id {
                        n.read = true;
                    }
                }
            });
            Ok(json!({}))
        }

        // ---- events ---------------------------------------------------------------------
        "events.read" => {
            let after = after_seq(server, p)?;
            let types = types(p);
            let limit = u(p, "limit").unwrap_or(500) as usize;
            let events = server
                .with_core(|c| c.store.events_after(after, limit, &types))
                .map_err(internal)?;
            let next = events.last().map(|e| e.seq).unwrap_or(after);
            Ok(json!({"events": events, "next": cursor(server, Some(next))}))
        }
        "events.wait" => {
            let after = after_seq(server, p)?;
            let types = types(p);
            let timeout = Duration::from_millis(u(p, "timeout_ms").unwrap_or(60_000));
            let mut rx = server.events.subscribe();
            if let Some(e) = server
                .with_core(|c| c.store.events_after(after, 1, &types))
                .map_err(internal)?
                .into_iter()
                .next()
            {
                return Ok(json!({"event": e}));
            }
            let deadline = Instant::now() + timeout;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match tokio::time::timeout(left, rx.recv()).await {
                    Ok(Ok(e))
                        if e.seq > after
                            && (types.is_empty()
                                || types.iter().any(|g| vk_store::glob_match(g, &e.kind))) =>
                    {
                        return Ok(json!({"event": *e}));
                    }
                    Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                        continue;
                    }
                    _ => return Err(err(ErrorKind::Timeout, "no matching event")),
                }
            }
        }
        "events.subscribe" | "events.unsubscribe" => Err(invalid(format!(
            "{method} must be called on a control connection"
        ))),

        // ---- blobs (search and layouts: parity.rs) --------------------------------------
        "blob.put" | "image.upload" => blob_put(server, p).inspect(|v| {
            if let Some(h) = v["hash"].as_str() {
                crate::blob_api::record_owner(server, ctx, h);
            }
            crate::blob_store::ingest_result(server, ctx, p, v);
        }),
        "blob.begin" => blob_begin(server, ctx, p),
        "blob.append" => blob_append(ctx, p),
        "blob.commit" => blob_commit(server, ctx, p).inspect(|v| {
            if let Some(h) = v["hash"].as_str() {
                crate::blob_api::record_owner(server, ctx, h);
            }
            crate::blob_store::ingest_result(server, ctx, p, v);
        }),
        "blob.abort" => blob_abort(ctx, p),
        "paste.translated" => crate::inbox::paste_translated(server, ctx, p),
        _ => Err(err(
            ErrorKind::MethodNotFound,
            format!("unknown method {method}"),
        )),
    }
}

fn types(p: &Value) -> Vec<String> {
    match p.get("types") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) => s.split(',').map(str::to_string).collect(),
        _ => vec![],
    }
}

/// `after` may be a full cursor (validated against the log identity) or a bare seq.
pub fn after_seq(server: &Server, p: &Value) -> Result<i64, RpcError> {
    let Some(a) = p.get("after").or_else(|| p.get("cursor")) else {
        return Ok(p.get("after_seq").and_then(Value::as_i64).unwrap_or(0));
    };
    if let Some(n) = a.as_i64() {
        return Ok(n);
    }
    let (sess, epoch, earliest) = server.with_core(|c| {
        (
            c.store.session_uuid.clone(),
            c.store.log_epoch.clone(),
            c.store.earliest_seq().unwrap_or(0),
        )
    });
    if a.get("session_uuid")
        .and_then(Value::as_str)
        .is_some_and(|x| x != sess)
        || a.get("log_epoch")
            .and_then(Value::as_str)
            .is_some_and(|x| x != epoch)
    {
        return Err(err(ErrorKind::Truncated, "cursor_epoch_mismatch")
            .details(json!({"current": cursor(server, None)})));
    }
    let seq = a.get("seq").and_then(Value::as_i64).unwrap_or(0);
    if seq > 0 && seq + 1 < earliest {
        return Err(err(ErrorKind::Truncated, "cursor older than retention")
            .details(json!({"earliest_seq": earliest})));
    }
    Ok(seq)
}

async fn send(server: &Server, ctx: &Ctx, pane: &str, bytes: Vec<u8>) -> Result<(), RpcError> {
    if bytes.is_empty() {
        return Ok(());
    }
    if let Some(reason) = server.agents.input_blocked(pane) {
        return Err(err(ErrorKind::Conflict, reason));
    }
    if ctx.pane_scope.is_none() {
        crate::sync_input::mirror(server, pane, &bytes);
    }
    let id = server.next_internal_input_id();
    match render::write_and_ack(server, pane, id, bytes).await {
        vk_proto::holder::InputStatus::ChildExited => {
            Err(err(ErrorKind::Conflict, "pane process exited"))
        }
        vk_proto::holder::InputStatus::Failed | vk_proto::holder::InputStatus::Unconfirmed => {
            Err(err(
                ErrorKind::Timeout,
                "input not confirmed: the pane's program hasn't read it (it may still arrive)",
            )
            .details(serde_json::json!({"status": "input_unconfirmed"})))
        }
        _ => Ok(()),
    }
}

/// `pane.read --source last-command` (03 §8): the last command's output from OSC 133 marks
/// (the previous command at an idle prompt, the running one otherwise), its last `lines` lines.
/// `no_marks` when the shell sent no prompt marks (`vibeke shell-integration` snippets).
fn read_last_command(server: &Server, pane: &str, lines: usize) -> R {
    let rt = server
        .pane_rt(pane)
        .ok_or_else(|| not_found("pane", pane))?;
    let (lc, rev) = {
        let sc = rt.screen.lock().unwrap();
        (sc.engine.last_command(), sc.rev)
    };
    let Some(lc) = lc else {
        return Err(err(
            ErrorKind::NotFound,
            "no OSC 133 prompt marks in this pane (no shell integration, or no command yet)",
        )
        .details(json!({"reason": "no_marks"})));
    };
    let mut out = String::new();
    for (i, r) in lc.rows.iter().enumerate() {
        let t = r.text();
        out.push_str(if r.wrapped { &t } else { t.trim_end() });
        if !r.wrapped && i + 1 < lc.rows.len() {
            out.push('\n');
        }
    }
    let all: Vec<&str> = out.lines().collect();
    let text = all[all.len().saturating_sub(lines)..].join("\n");
    Ok(json!({
        "text": text,
        "revision": rev,
        "source": "last-command",
        "running": lc.running,
        "exit_code": lc.exit,
        "prompt_line": lc.prompt_line,
    }))
}

pub fn read_text(server: &Server, pane: &str, source: &str, lines: usize) -> String {
    let Some(rt) = server.pane_rt(pane) else {
        return String::new();
    };
    let sc = rt.screen.lock().unwrap();
    let e = &sc.engine;
    let mut rows: Vec<vk_proto::render::Row> = Vec::new();
    match source {
        "visible" | "detection" => rows = e.visible_rows(),
        _ => {
            let h = e.history_len();
            let want_hist = lines.saturating_sub(e.rows() as usize).min(h);
            for i in h - want_hist..h {
                if let Some(r) = e.history_row(i) {
                    rows.push(r);
                }
            }
            rows.extend(e.visible_rows());
        }
    }
    // Drop trailing blank rows.
    while rows.last().is_some_and(|r| r.text().trim().is_empty()) {
        rows.pop();
    }
    let unwrap = source == "recent_unwrapped" || source == "scrollback";
    let mut out = String::new();
    for (i, r) in rows.iter().enumerate() {
        let t = r.text();
        out.push_str(if r.wrapped && unwrap {
            &t
        } else {
            t.trim_end()
        });
        if !(r.wrapped && unwrap) && i + 1 < rows.len() {
            out.push('\n');
        }
    }
    let all: Vec<&str> = out.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub async fn wait_idle(
    server: &Server,
    pane: &str,
    quiet: Duration,
    timeout: Duration,
    changed_since: Option<u64>,
) -> Result<u64, RpcError> {
    let rt = server
        .pane_rt(pane)
        .ok_or_else(|| not_found("pane", pane))?;
    let deadline = Instant::now() + timeout;
    let mut rx = rt.rev_tx.subscribe();
    let mut seen_change = changed_since.is_none();
    loop {
        let rev = rt.rev();
        if changed_since.is_some_and(|r| rev > r) {
            seen_change = true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(err(ErrorKind::Timeout, "pane not idle").details(json!({"revision": rev})));
        }
        match tokio::time::timeout(quiet.min(left), rx.changed()).await {
            Err(_) if seen_change => return Ok(rev),
            Err(_) => {}
            Ok(Ok(())) => seen_change = true,
            Ok(Err(_)) => return Err(err(ErrorKind::Conflict, "pane closed")),
        }
    }
}

/// Store an uploaded file (base64) under the pane inbox (06 A11.4) and return its path on this
/// machine. Content-addressed: `<blake3-12>/<basename>`.
pub fn blob_put(server: &Server, p: &Value) -> R {
    use base64::Engine;
    let _ = server;
    let data = match (s(p, "data_b64"), s(p, "path")) {
        (Some(d), _) => base64::engine::general_purpose::STANDARD
            .decode(d)
            .map_err(|e| invalid(e.to_string()))?,
        (None, Some(path)) => {
            std::fs::read(path).map_err(|e| invalid(format!("read {path}: {e}")))?
        }
        _ => return Err(invalid("data_b64 or path required")),
    };
    let hash = blake3::hash(&data).to_hex().to_string();
    let name = s(p, "name")
        .map(|n| n.rsplit('/').next().unwrap_or(n).to_string())
        .filter(|n| !n.is_empty() && n != "." && n != "..")
        .unwrap_or_else(|| {
            let ext = match s(p, "mime").unwrap_or("") {
                "image/png" => "png",
                "image/jpeg" => "jpg",
                "image/gif" => "gif",
                _ => "bin",
            };
            format!("clipboard-{}.{ext}", vk_store::now_ms())
        });
    let dir = crate::paths::Paths::inbox().join(&hash[..12]);
    write_private(&dir, &name, &data).map_err(internal)?;
    let path = dir.join(&name);
    Ok(json!({"hash": hash, "size": data.len(), "path_on_machine": path, "path": path}))
}

// ---- chunked uploads (06 A11): blob.begin / blob.append / blob.commit / blob.abort ----------

/// Largest decoded chunk accepted by `blob.append`; keeps every frame far below the 64 MiB limit.
pub const BLOB_MAX_CHUNK: usize = 1 << 20;
/// Concurrent in-flight uploads across all connections.
const BLOB_MAX_UPLOADS: usize = 16;
/// An upload idle this long is abandoned and its staging file removed.
const BLOB_IDLE: Duration = Duration::from_secs(600);

struct Upload {
    owner: String,
    name: String,
    size: u64,
    sha256: Option<String>,
    staged: std::path::PathBuf,
    file: std::fs::File,
    offset: u64,
    blake: blake3::Hasher,
    sha: sha2::Sha256,
    last: Instant,
}

type UploadMap = std::sync::Mutex<std::collections::HashMap<String, Upload>>;

fn uploads() -> &'static UploadMap {
    static U: std::sync::OnceLock<UploadMap> = std::sync::OnceLock::new();
    U.get_or_init(Default::default)
}

fn blob_limit() -> u64 {
    vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c.paste.max_auto_bytes.0)
        .unwrap_or(vk_config::ByteSize::mib(50).0)
}

fn blob_begin(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let _ = server;
    upload_begin(
        &crate::paths::Paths::inbox(),
        &ctx.client_id,
        blob_limit(),
        p,
    )
}
fn blob_append(ctx: &Ctx, p: &Value) -> R {
    upload_append(&ctx.client_id, p)
}
fn blob_commit(server: &Server, ctx: &Ctx, p: &Value) -> R {
    // `stage: "browser"`: a copy of a file the user confirmed for a browser pane's page goes
    // to the private drop directory, the only place pages get files from (06 B3.2).
    if s(p, "stage") == Some("browser") {
        let root = crate::browser_pane::page_io::ensure_drops_root(server).map_err(internal)?;
        return upload_commit(&root, &ctx.client_id, p);
    }
    // `unpack: "tar"`: a dropped directory, sent as a tar stream (06 A11.2).
    if s(p, "unpack") == Some("tar") {
        return crate::inbox::commit_tar(
            &crate::paths::Paths::inbox(),
            &ctx.client_id,
            blob_limit(),
            p,
        );
    }
    upload_commit(&crate::paths::Paths::inbox(), &ctx.client_id, p)
}
fn blob_abort(ctx: &Ctx, p: &Value) -> R {
    upload_abort(&ctx.client_id, p)
}

fn upload_drop(u: Upload) {
    let _ = std::fs::remove_file(&u.staged);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn upload_begin(inbox: &std::path::Path, owner: &str, limit: u64, p: &Value) -> R {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let size = u(p, "size").ok_or_else(|| invalid("missing param `size`"))?;
    if size > limit {
        return Err(invalid(format!(
            "upload of {size} bytes exceeds the {limit} byte limit (paste.max_auto_bytes)"
        ))
        .details(json!({"limit": limit, "size": size})));
    }
    let name = s(p, "name")
        .map(|n| n.rsplit('/').next().unwrap_or(n).to_string())
        .filter(|n| !n.is_empty() && n != "." && n != ".." && !n.contains('\0'))
        .ok_or_else(|| invalid("missing or invalid `name`"))?;
    let sha256 = s(p, "sha256").map(|h| h.to_ascii_lowercase());
    if let Some(h) = &sha256
        && (h.len() != 64 || !h.bytes().all(|c| c.is_ascii_hexdigit()))
    {
        return Err(invalid("sha256 must be 64 hex digits"));
    }
    let mut map = uploads().lock().unwrap();
    let stale: Vec<String> = map
        .iter()
        .filter(|(_, x)| x.last.elapsed() > BLOB_IDLE)
        .map(|(k, _)| k.clone())
        .collect();
    for k in stale {
        if let Some(x) = map.remove(&k) {
            upload_drop(x);
        }
    }
    if map.len() >= BLOB_MAX_UPLOADS {
        return Err(err(ErrorKind::RateLimited, "too many uploads in flight"));
    }
    let id = format!("up-{:x}-{:x}", vk_store::now_ms(), rand_u64());
    let dir = inbox.join(".incoming");
    std::fs::create_dir_all(&dir).map_err(internal)?;
    let _ = std::fs::set_permissions(inbox, std::fs::Permissions::from_mode(0o700));
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let staged = dir.join(&id);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)
        .map_err(internal)?;
    map.insert(
        id.clone(),
        Upload {
            owner: owner.to_string(),
            name,
            size,
            sha256,
            staged,
            file,
            offset: 0,
            blake: blake3::Hasher::new(),
            sha: sha2::Digest::new(),
            last: Instant::now(),
        },
    );
    Ok(json!({"upload_id": id, "max_chunk": BLOB_MAX_CHUNK}))
}

fn rand_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

pub fn upload_append(owner: &str, p: &Value) -> R {
    use base64::Engine;
    let id = req(p, "upload_id")?;
    let offset = u(p, "offset").ok_or_else(|| invalid("missing param `offset`"))?;
    let b64 = req(p, "data_b64")?;
    // Reject before decoding: base64 is 4/3 of the decoded size.
    if b64.len() > BLOB_MAX_CHUNK / 3 * 4 + 8 {
        return Err(invalid(format!(
            "chunk exceeds the {BLOB_MAX_CHUNK} byte maximum"
        )));
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| invalid(e.to_string()))?;
    if data.len() > BLOB_MAX_CHUNK {
        return Err(invalid(format!(
            "chunk of {} bytes exceeds the {BLOB_MAX_CHUNK} byte maximum",
            data.len()
        )));
    }
    let mut map = uploads().lock().unwrap();
    let up = match map.get_mut(id) {
        Some(x) if x.owner == owner => x,
        _ => return Err(not_found("upload", id)),
    };
    if offset != up.offset {
        return Err(err(
            ErrorKind::Conflict,
            format!("offset mismatch: expected {}, got {offset}", up.offset),
        )
        .details(json!({"expected": up.offset})));
    }
    if up.offset + data.len() as u64 > up.size {
        let x = map.remove(id).unwrap();
        upload_drop(x);
        return Err(invalid("upload exceeds its declared size"));
    }
    if let Err(e) = std::io::Write::write_all(&mut up.file, &data) {
        let x = map.remove(id).unwrap();
        upload_drop(x);
        return Err(internal(e));
    }
    up.blake.update(&data);
    sha2::Digest::update(&mut up.sha, &data);
    up.offset += data.len() as u64;
    up.last = Instant::now();
    Ok(json!({"offset": up.offset}))
}

pub fn upload_commit(inbox: &std::path::Path, owner: &str, p: &Value) -> R {
    use std::os::unix::fs::PermissionsExt;
    let id = req(p, "upload_id")?;
    let mut map = uploads().lock().unwrap();
    match map.get(id) {
        Some(x) if x.owner == owner => {}
        _ => return Err(not_found("upload", id)),
    }
    let up = map.remove(id).unwrap();
    drop(map);
    if up.offset != up.size {
        let (got, want) = (up.offset, up.size);
        upload_drop(up);
        return Err(invalid(format!(
            "incomplete upload: {got} of {want} bytes received"
        )));
    }
    let sha_hex = hex(&sha2::Digest::finalize(up.sha.clone()));
    if up.sha256.as_deref().is_some_and(|h| h != sha_hex) {
        upload_drop(up);
        return Err(invalid("sha256 mismatch"));
    }
    let hash = up.blake.finalize().to_hex().to_string();
    let dir = inbox.join(&hash[..12]);
    let path = dir.join(&up.name);
    let res = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        up.file.sync_all()?;
        std::fs::rename(&up.staged, &path)
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&up.staged);
        return Err(internal(e));
    }
    Ok(
        json!({"hash": hash, "sha256": sha_hex, "size": up.size, "path_on_machine": path, "path": path}),
    )
}

pub fn upload_abort(owner: &str, p: &Value) -> R {
    let id = req(p, "upload_id")?;
    let mut map = uploads().lock().unwrap();
    if map.get(id).is_some_and(|x| x.owner == owner)
        && let Some(x) = map.remove(id)
    {
        upload_drop(x);
    }
    Ok(json!({"aborted": true}))
}

fn write_private(dir: &std::path::Path, name: &str, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::create_dir_all(dir)?;
    let root = crate::paths::Paths::inbox();
    let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let path = dir.join(name);
    if path.exists() && std::fs::metadata(&path)?.len() == data.len() as u64 {
        return Ok(()); // same content-addressed file already present
    }
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    std::io::Write::write_all(&mut f, data)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod blob_upload_tests {
    use super::*;
    use base64::Engine;

    fn b64(d: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(d)
    }
    fn id_of(r: R) -> String {
        r.unwrap()["upload_id"].as_str().unwrap().to_string()
    }

    #[test]
    fn begin_append_commit_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path();
        let data: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let sha = hex(&<sha2::Sha256 as sha2::Digest>::digest(&data));
        let id = id_of(upload_begin(
            inbox,
            "t1",
            10_000,
            &json!({"name": "../evil/a.bin", "size": data.len(), "sha256": sha}),
        ));
        let chunk =
            |off: u64, d: &[u8]| json!({"upload_id": id, "offset": off, "data_b64": b64(d)});
        // Wrong owner cannot touch it.
        assert!(upload_append("other", &chunk(0, &data[..10])).is_err());
        let r = upload_append("t1", &chunk(0, &data[..1000])).unwrap();
        assert_eq!(r["offset"], 1000);
        // Offset mismatch is a conflict and leaves the upload intact.
        let e = upload_append("t1", &chunk(5, &data[1000..])).unwrap_err();
        assert_eq!(e.code, ErrorKind::Conflict.code());
        // Committing early fails.
        let early = id_of(upload_begin(
            inbox,
            "t1",
            10_000,
            &json!({"name": "x", "size": 4}),
        ));
        assert!(upload_commit(inbox, "t1", &json!({"upload_id": early})).is_err());
        upload_append("t1", &chunk(1000, &data[1000..])).unwrap();
        let r = upload_commit(inbox, "t1", &json!({"upload_id": id})).unwrap();
        let path = std::path::PathBuf::from(r["path"].as_str().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert_eq!(path.file_name().unwrap(), "a.bin");
        let hash = blake3::hash(&data).to_hex().to_string();
        assert_eq!(
            path.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            &hash[..12]
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(upload_commit(inbox, "t1", &json!({"upload_id": id})).is_err());
    }

    #[test]
    fn limits_and_integrity() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path();
        // Over the configured limit at begin.
        assert!(upload_begin(inbox, "t2", 100, &json!({"name": "big", "size": 101})).is_err());
        // Oversized chunk.
        let id = id_of(upload_begin(
            inbox,
            "t2",
            u64::MAX,
            &json!({"name": "c", "size": 3u64 << 20}),
        ));
        let big = vec![0u8; BLOB_MAX_CHUNK + 1];
        assert!(
            upload_append(
                "t2",
                &json!({"upload_id": id, "offset": 0, "data_b64": b64(&big)})
            )
            .is_err()
        );
        // Exceeding the declared size drops the upload.
        let id2 = id_of(upload_begin(
            inbox,
            "t2",
            100,
            &json!({"name": "d", "size": 4}),
        ));
        assert!(
            upload_append(
                "t2",
                &json!({"upload_id": id2, "offset": 0, "data_b64": b64(b"12345")})
            )
            .is_err()
        );
        assert!(
            upload_append(
                "t2",
                &json!({"upload_id": id2, "offset": 0, "data_b64": b64(b"1")})
            )
            .is_err()
        );
        // Bad checksum.
        let id3 = id_of(upload_begin(
            inbox,
            "t2",
            100,
            &json!({"name": "e", "size": 2, "sha256": "0".repeat(64)}),
        ));
        upload_append(
            "t2",
            &json!({"upload_id": id3, "offset": 0, "data_b64": b64(b"ab")}),
        )
        .unwrap();
        assert!(upload_commit(inbox, "t2", &json!({"upload_id": id3})).is_err());
        // Abort removes the staging file.
        let id4 = id_of(upload_begin(
            inbox,
            "t2",
            100,
            &json!({"name": "f", "size": 2}),
        ));
        assert!(inbox.join(".incoming").join(&id4).exists());
        upload_abort("t2", &json!({"upload_id": id4})).unwrap();
        assert!(!inbox.join(".incoming").join(&id4).exists());
        upload_abort("t2", &json!({"upload_id": id})).unwrap();
    }
}
