//! The app API (spec 16 §7.2–§7.6): scope checks, operation ids, normalization and the method table.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use vk_proto::rpc::RpcError;

use crate::Gateway;
use crate::state::{Device, Scope, now_s};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub kind: String,
    pub message: String,
    pub details: Value,
}

impl ApiError {
    pub fn new(kind: &str, message: impl Into<String>) -> Self {
        ApiError {
            kind: kind.into(),
            message: message.into(),
            details: Value::Null,
        }
    }
    pub fn unavailable(m: impl Into<String>) -> Self {
        Self::new("unavailable", m)
    }
    pub fn invalid(m: impl Into<String>) -> Self {
        Self::new("invalid_params", m)
    }
    pub fn from_server(e: RpcError) -> Self {
        ApiError {
            kind: e.data.kind,
            message: e.message,
            details: e.data.details,
        }
    }
    fn code(&self) -> i64 {
        match self.kind.as_str() {
            "invalid_params" => -32602,
            "method_not_found" => -32601,
            "forbidden" => -32003,
            "not_found" => -32004,
            "conflict" | "stale" => -32009,
            "unavailable" => -32010,
            _ => -32000,
        }
    }
    pub fn to_json(&self) -> Value {
        json!({"code": self.code(), "message": self.message, "data": {"kind": self.kind, "details": self.details}})
    }
}

pub type ApiResult = Result<Value, ApiError>;

/// Server methods without side effects (everything else is re-authorized before it runs).
const SERVER_READ_ONLY: &[&str] = &[
    "client.list",
    "session.snapshot",
    "notification.list",
    "pane.read",
    "pane.get",
    "agent.get",
    "agent.list",
    "agent.transcript",
    "agent.harnesses",
    "agent.commands",
    "agent.models",
    "agent.wait",
    "interaction.get",
    "interaction.list",
    "git.status",
    "git.diff",
    "git.log",
    "fs.list",
    "fs.read",
    "server.status",
    "attention.list",
    "task.get",
    "task.review.get",
    "task.review.candidates",
    "task.review.diff",
    "task.check.list",
    "task.check.get",
    "preview.list",
    "preview.get",
    "preview.url",
    "preview.status",
    "screenshot.list",
    "screenshot.get",
    "worktree.list",
    "fs.browse",
    "repo.candidates",
    "handoff.incoming.list",
    "handoff.incoming.get",
    "handoff.jobs",
    "handoff.peers",
    "agent.turns",
    "assistant.status",
    "assistant.get",
    "desk.search",
    "search.query",
    "sandbox.list",
    "sandbox.status",
    "browser.list",
    "browser.status",
    "browser.screencast_frame",
];

/// Assistant operations apps may start (`assistant.generate`): catch-up summaries, decision
/// cards and suggested replies. Each still needs the workspace's consent on the host, and a
/// preview the app confirms unless the host's config auto-sends that operation.
pub const APP_ASSIST_OPS: &[&str] = &[
    "briefing",
    "background_summary",
    "decision_card",
    "reply_suggestions",
];

/// Minimum scope per method; `None` = unknown method.
pub fn required_scope(method: &str) -> Option<Scope> {
    use Scope::*;
    Some(match method {
        "hello" | "ping" | "client.visibility" | "dashboard.get" | "pane.read"
        | "agent.transcript" | "agent.harnesses" | "interaction.list" | "interaction.get"
        | "git.status" | "git.diff" | "notification.list" | "events.subscribe" | "prefs.get"
        | "prefs.set" | "push.subscribe" | "push.unsubscribe" | "push.test" | "devices.list" => {
            View
        }
        // A fresh relay admission ticket for the caller (spec 16 §6.6).
        "relay.ticket" => View,
        // Workspace views (read-only passthroughs).
        "attention.list"
        | "task.review.get"
        | "task.review.candidates"
        | "task.review.diff"
        | "task.check.list"
        | "task.check.get"
        | "preview.list"
        | "preview.get"
        | "preview.url"
        | "preview.status"
        | "screenshot.list"
        | "screenshot.get"
        | "worktree.list"
        | "git.log"
        | "fs.list"
        | "fs.read" => View,
        // Pickers: the slash-command catalog and the model list are reads; switching the model
        // changes what the agent runs, like a prompt.
        "agent.commands" | "agent.models" => View,
        "agent.set_model" => Full,
        "agent.interrupt"
        | "interaction.answer"
        | "interaction.answer_batch"
        | "notification.read"
        | "attention.update" => Approve,
        "pane.send_text"
        | "pane.send_keys"
        | "pane.rename"
        | "pane.close"
        | "pane.focus"
        | "agent.prompt"
        | "agent.start"
        | "tab.create"
        | "attachment.put"
        | "stt.transcribe"
        | "devices.revoke"
        | "share.create"
        | "handoff.incoming.list"
        | "handoff.incoming.get"
        | "handoff.accept"
        | "handoff.decline"
        | "handoff.resume"
        | "handoff.prefs"
        | "task.check.run"
        | "preview.open"
        | "preview.promote"
        | "preview.forget"
        | "tab.rename"
        | "tab.close"
        | "tab.focus" => Full,
        // Host-wide directory browsing for path pickers: read-only, but sees all of $HOME.
        "fs.browse" | "repo.candidates" => Full,
        // Gateway-to-gateway handoffs (spec 16 §15.2): a peer delivers (handoff_peer.rs); the
        // owner's apps start, follow and cancel outgoing jobs (server records, handoff_send.rs).
        "handoff.offer" | "handoff.status" | "handoff.write" | "handoff.commit"
        | "handoff.discard" | "handoff.send" | "handoff.jobs" | "handoff.cancel"
        | "handoff.peers" => Full,
        // Host-to-host trust and invitation management (spec 16 §15.3–§15.4, peers.rs).
        "peer.invite" | "peer.redeem" | "peer.list" | "peer.remove" | "share.list"
        | "share.revoke" => Full,
        // Approved calls (09 §3.2): a pane asks, the owner's apps review and decide.
        "auth.list" | "auth.approve.decide" => Full,
        // Catch-up, search, sandboxes and live browser previews: reads. Attaching to a
        // screencast only counts this device as a viewer (screencast.rs).
        "agent.turns"
        | "assistant.status"
        | "assistant.get"
        | "desk.search"
        | "search.query"
        | "sandbox.list"
        | "sandbox.status"
        | "browser.list"
        | "browser.status"
        | "browser.attach_screencast"
        | "browser.screencast_frame"
        | "browser.detach_screencast" => View,
        // New agents in a new folder or worktree. The assistant costs money and sends content
        // to a model provider. Taking over a browser session
        // drives it instead of the agent.
        "worktree.create" | "workspace.create" | "assistant.generate" | "assistant.confirm"
        | "assistant.cancel" | "browser.take_over" | "browser.release" | "browser.click"
        | "browser.type" | "browser.press" | "browser.navigate" => Full,
        _ => return None,
    })
}

/// Methods that act on or read the whole host rather than a pane, workspace, run or task:
/// refused to every device limited to a pane or workspace (`check_limit`).
const HOST_WIDE: &[&str] = &[
    "share.create",
    "share.list",
    "share.revoke",
    "devices.revoke",
    "auth.list",
    "auth.approve.decide",
    // A new workspace or worktree is outside any shared one.
    "worktree.create",
    "workspace.create",
    // The desk index covers every repository and harness session on the host.
    "desk.search",
];

/// Method families that are host-wide as a whole: the assistant reads
/// across workspaces and spends the owner's budget, browser sessions and sandboxes are not tied
/// to a shared pane in a way the gateway can check.
const HOST_WIDE_PREFIXES: &[&str] = &[
    "peer.",
    "handoff.",
    "assistant.",
    "browser.",
    "sandbox.",
    "desk.",
];

pub fn host_wide(method: &str) -> bool {
    HOST_WIDE.contains(&method) || HOST_WIDE_PREFIXES.iter().any(|p| method.starts_with(p))
}

/// Full-scope methods without side effects (no op_id needed).
const FULL_READ_ONLY: &[&str] = &[
    "handoff.status",
    "handoff.jobs",
    "handoff.peers",
    "handoff.incoming.list",
    "handoff.incoming.get",
    "peer.list",
    "share.list",
    "fs.browse",
    "repo.candidates",
    "auth.list",
];

pub fn is_mutating(method: &str) -> bool {
    // Chunk reads have no side effect; caching them would hold whole bundles in memory.
    !FULL_READ_ONLY.contains(&method)
        && matches!(required_scope(method), Some(Scope::Approve | Scope::Full))
        || matches!(
            method,
            "push.subscribe" | "push.unsubscribe" | "push.test" | "prefs.set"
        )
}

// ---------------------------------------------------------------------------------------------
// Normalization (spec 16 §7.5): server enums are PascalCase except `interaction.list`'s kind.

const ENUM_KEYS: &[&str] = &[
    "kind",
    "status",
    "delivery",
    "risk",
    "decision",
    "answer_channel",
    "source",
    "health",
    "value",
];

fn snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn is_pascal(s: &str) -> bool {
    s.len() <= 40
        && s.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Rewrite enum-valued fields to snake_case, recursively.
pub fn normalize(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, val) in m.iter_mut() {
                if ENUM_KEYS.contains(&k.as_str())
                    && let Value::String(s) = val
                    && is_pascal(s)
                {
                    *s = snake(s);
                }
                normalize(val);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(normalize),
        _ => {}
    }
}

fn s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(|v| v.as_str())
}

fn req<'a>(p: &'a Value, k: &str) -> Result<&'a str, ApiError> {
    s(p, k)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::invalid(format!("{k} is required")))
}

/// Like [`pick`] but keeps explicit nulls (`attention.update {snooze_until_ms: null}` clears).
fn pick_nullable(p: &Value, keys: &[&str]) -> Value {
    let mut m = Map::new();
    for k in keys {
        if let Some(v) = p.get(*k) {
            m.insert((*k).into(), v.clone());
        }
    }
    Value::Object(m)
}

fn pick(p: &Value, keys: &[&str]) -> Value {
    let mut m = Map::new();
    for k in keys {
        if let Some(v) = p.get(*k)
            && !v.is_null()
        {
            m.insert((*k).into(), v.clone());
        }
    }
    Value::Object(m)
}

// ---------------------------------------------------------------------------------------------
// Batch eligibility (spec 16 §7.6)

pub fn fingerprint(it: &Value) -> Option<String> {
    let a = it.get("action")?;
    // Exact command bytes: collapsing whitespace would merge `a b` with `a\nb` (two commands).
    let target = match s(a, "command").filter(|c| !c.trim().is_empty()) {
        Some(c) => c.to_string(),
        None => {
            let mut paths: Vec<&str> = a
                .get("paths")?
                .as_array()?
                .iter()
                .filter_map(|p| p.as_str())
                .collect();
            paths.sort();
            paths.join("\n")
        }
    };
    Some(format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        s(it, "harness").unwrap_or(""),
        s(a, "tool").unwrap_or(""),
        target,
        s(it, "repo_root").unwrap_or("")
    ))
}

pub fn batch_eligible(it: &Value) -> bool {
    // Sandbox boundary requests (a push, a file copied out) are always decided one by one.
    it.pointer("/action/tool").and_then(|t| t.as_str()) != Some("boundary")
        && s(it, "kind") == Some("approval")
        && s(it, "status") == Some("open")
        && it.get("answerable").and_then(|v| v.as_bool()) == Some(true)
        && matches!(
            it.pointer("/action/risk").and_then(|r| r.as_str()),
            Some("low" | "medium")
        )
}

// ---------------------------------------------------------------------------------------------

/// What a limited (share) device may touch (spec 16 §15.1).
#[derive(Debug, Clone)]
pub struct Allowed {
    pub workspace: Option<String>,
    pub pane: Option<String>,
}

impl Allowed {
    pub fn of(d: &Device) -> Option<Allowed> {
        d.limit.as_ref().map(|l| Allowed {
            workspace: l.workspace.clone(),
            pane: l.pane.clone(),
        })
    }
    /// A pane JSON object (`{id, workspace}`) inside the limit?
    pub fn pane_ok(&self, pane_id: &str, workspace: Option<&str>) -> bool {
        match (&self.pane, &self.workspace) {
            (Some(p), _) => p == pane_id,
            (None, Some(w)) => workspace == Some(w.as_str()),
            (None, None) => true,
        }
    }
    /// A task (by its workspace) inside the limit? Tasks belong to workspaces, so a pane-only
    /// share sees none.
    pub fn task_ok(&self, workspace: Option<&str>) -> bool {
        match (&self.pane, &self.workspace) {
            (Some(_), _) => false,
            (None, Some(w)) => workspace == Some(w.as_str()),
            (None, None) => true,
        }
    }
    /// A tab inside the limit? A pane-only share sees only the tab holding its pane.
    pub fn tab_ok(&self, workspace: Option<&str>, panes: &[String]) -> bool {
        match (&self.pane, &self.workspace) {
            (Some(p), _) => panes.iter().any(|x| x == p),
            (None, Some(w)) => workspace == Some(w.as_str()),
            (None, None) => true,
        }
    }
    /// An event subject inside the limit?
    pub fn subject_ok(&self, subject: &Value) -> bool {
        let pane = s(subject, "pane");
        let ws = s(subject, "workspace");
        match (&self.pane, &self.workspace) {
            (Some(p), _) => pane == Some(p.as_str()),
            (None, Some(w)) => ws == Some(w.as_str()),
            (None, None) => true,
        }
    }
}

/// Methods never available to share devices.
pub fn kind_allows(kind: &str, method: &str) -> bool {
    match kind {
        // Another Vibeke host (spec 16 §15.3): delivers handoffs, nothing else.
        "peer" => matches!(
            method,
            "hello"
                | "ping"
                | "relay.ticket"
                | "handoff.offer"
                | "handoff.status"
                | "handoff.write"
                | "handoff.commit"
                | "handoff.discard"
        ),
        "share" => {
            !(method.starts_with("devices.")
                || method.starts_with("share.")
                || method.starts_with("peer.")
                || method.starts_with("handoff.")
                || method.starts_with("auth.")
                // Push belongs to the user's own devices: a share device must not hand this
                // host a VAPID private key or trigger pushes (the app only syncs own hosts).
                || method.starts_with("push.")
                || method.starts_with("tab.") && method != "tab.create"
                || host_wide(method)
                || matches!(
                    method,
                    "stt.transcribe"
                        | "task.check.run"
                        | "attention.update"
                        | "preview.status"
                        | "fs.browse"
                        | "repo.candidates"
                ))
        }
        "device" => true,
        // An unknown kind (a newer gateway's registry) gets nothing.
        _ => false,
    }
}

pub struct Call<'a> {
    pub gw: &'a Arc<Gateway>,
    pub device: &'a Device,
}

impl Call<'_> {
    async fn server(&self, method: &str, params: Value) -> ApiResult {
        // Re-check authorization right before every side effect: the device may have been
        // revoked (here or by `vibeke-gateway revoke`) while this request was queued.
        if !SERVER_READ_ONLY.contains(&method) {
            self.still_authorized()?;
        }
        if SERVER_READ_ONLY.contains(&method) {
            return self.gw.server.call(method, params).await;
        }
        self.gw.server.call_as(&self.actor(), method, params).await
    }

    fn still_authorized(&self) -> Result<(), ApiError> {
        let _ = self.gw.reload_devices();
        if self.gw.device(&self.device.id).is_none_or(|d| d.expired()) {
            return Err(ApiError::new(
                "forbidden",
                "this device is no longer authorized",
            ));
        }
        Ok(())
    }

    pub fn actor(&self) -> String {
        format!("gateway:{}", self.device.name)
    }

    async fn interaction(&self, id: &str) -> ApiResult {
        let mut v = self
            .server("interaction.get", json!({"interaction": id}))
            .await?;
        let mut it = v
            .get_mut("interaction")
            .map(Value::take)
            .unwrap_or(Value::Null);
        normalize(&mut it);
        self.enrich(&mut it).await;
        Ok(it)
    }

    /// Add `harness` and `repo_root` for grouping, and `boundary` for a sandbox boundary request.
    async fn enrich(&self, it: &mut Value) {
        if let Some(b) = boundary_of(it) {
            it["boundary"] = b;
        }
        if let Some(run) = s(it, "run").map(str::to_string)
            && let Ok(r) = self.server("agent.get", json!({"target": run})).await
        {
            if let Some(h) = r.pointer("/run/harness").cloned() {
                it["harness"] = h;
            }
            if let Some(cwd) = r.pointer("/run/cwd").and_then(|c| c.as_str()) {
                it["repo_root"] = Value::from(repo_root(cwd));
            }
        }
    }

    /// Answer one interaction the caller decided on at revision `rev`. With `group`, the item must
    /// still be batch-eligible with that fingerprint at execution time (spec 16 §7.6).
    async fn answer_one(
        &self,
        id: &str,
        rev: u64,
        p: &Value,
        op_id: &str,
        group: Option<&str>,
    ) -> ApiResult {
        let it = self.interaction(id).await?;
        let open = s(&it, "status") == Some("open");
        let rev_ok = it.get("decision_rev").and_then(|v| v.as_u64()) == Some(rev);
        let group_ok =
            group.is_none_or(|g| batch_eligible(&it) && fingerprint(&it).as_deref() == Some(g));
        if !open || !rev_ok || !group_ok {
            return Err(stale(&it));
        }
        // A boundary request runs once on "allow" (sandbox_boundary.rs): no standing rule, no
        // text answer.
        if it.get("boundary").is_some()
            && (!matches!(s(p, "decision"), Some("allow" | "deny"))
                || p.get("choices").is_some()
                || p.get("text").is_some())
        {
            return Err(ApiError::invalid(
                "a sandbox boundary request is answered allow (this once) or deny",
            ));
        }
        let mut params = pick(p, &["decision", "choices", "text"]);
        params["interaction"] = id.into();
        params["idempotency_key"] = format!("gw:{}:{op_id}:{id}", self.device.id).into();
        params["actor"] = self.actor().into();
        // The server re-checks the revision under its decision lock.
        params["expected_decision_rev"] = rev.into();
        let mut r = match self.server("interaction.answer", params).await {
            Err(e) if e.message.starts_with("stale") => return Err(stale(&it)),
            other => other?,
        };
        normalize(&mut r);
        Ok(r)
    }

    /// Pane id + workspace of a pane, run or interaction target.
    async fn locate(&self, p: &Value) -> Result<Option<(String, Option<String>)>, ApiError> {
        let pane = if let Some(pane) = s(p, "pane") {
            Some(pane.to_string())
        } else if let Some(t) = s(p, "target") {
            let r = self.server("agent.get", json!({"target": t})).await?;
            r.pointer("/run/pane")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        } else if let Some(i) = s(p, "interaction") {
            let r = self
                .server("interaction.get", json!({"interaction": i}))
                .await?;
            r.pointer("/interaction/pane")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        } else {
            None
        };
        let Some(pane) = pane else { return Ok(None) };
        let r = self.server("pane.get", json!({"pane": pane})).await?;
        let id = r
            .pointer("/pane/id")
            .and_then(|v| v.as_str())
            .unwrap_or(&pane)
            .to_string();
        let ws = r
            .pointer("/pane/workspace")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(Some((id, ws)))
    }

    /// Enforce a share device's limit before dispatching (spec 16 §15.1).
    async fn check_limit(
        &self,
        allowed: &Allowed,
        method: &str,
        p: &Value,
    ) -> Result<(), ApiError> {
        let deny = || ApiError::new("forbidden", "outside what was shared with you");
        // A selector is resolved on the local machine only: a `machine` parameter could make the
        // same handle (e.g. a preview id) name something on another machine after the check.
        if p.get("machine").is_some_and(|m| !m.is_null()) {
            return Err(deny());
        }
        // A named workspace must be the shared one, whatever else the call names (`tab.create`
        // with a shared `pane` and another `workspace` creates the tab in that workspace).
        if s(p, "workspace").is_some_and(|w| Some(w) != allowed.workspace.as_deref()) {
            return Err(deny());
        }
        match method {
            // A new worktree or folder is a new workspace, outside the shared one.
            "agent.start" if p.get("worktree").is_some() || p.get("new_workspace").is_some() => {
                return Err(deny());
            }
            "tab.create" | "agent.start" if s(p, "pane").is_none() => {
                if allowed.pane.is_some() || s(p, "workspace") != allowed.workspace.as_deref() {
                    return Err(deny());
                }
            }
            // Opens beside whatever the owner has focused, or a host browser window.
            "preview.open" => return Err(deny()),
            // Host-wide do-not-disturb silences every device's approval pushes.
            "prefs.set" if p.get("host").is_some() => return Err(deny()),
            // Only the shared run's session model, never the persistent default for every run.
            "agent.set_model" if s(p, "scope").is_some_and(|sc| sc != "session") => {
                return Err(deny());
            }
            "interaction.answer_batch" => {
                for item in p
                    .get("items")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    self.check_selectors(allowed, item).await?;
                }
            }
            // Host-wide management: a share (even a Control share, full scope within its pane or
            // workspace) never creates or revokes access, moves work between hosts or decides
            // approvals for other panes.
            m if host_wide(m) => {
                return Err(deny());
            }
            "worktree.list" | "fs.browse" | "repo.candidates" => {
                // The list covers the whole repository (sibling checkouts, paths, branches), so
                // limited devices can't call it at all.
                return Err(deny());
            }
            "notification.read" => {
                // Only a notification attached to a shared pane, never "all".
                if p.get("all").is_some_and(|v| v != &Value::Bool(false)) {
                    return Err(deny());
                }
                let id = s(p, "notification").ok_or_else(deny)?;
                let list = self
                    .server("notification.list", json!({"limit": 500}))
                    .await?;
                let pane = list
                    .get("notifications")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .find(|n| s(n, "id") == Some(id))
                    .and_then(|n| s(n, "pane").map(str::to_string))
                    .ok_or_else(deny)?;
                self.check_selectors(allowed, &json!({"pane": pane}))
                    .await?;
            }
            _ => {}
        }
        self.check_selectors(allowed, p).await
    }

    /// Every selector present (`pane`, `target`, `run`, `interaction`, `task`, `check_run`,
    /// `tab`, `preview`) must resolve inside the limit, so a request can't pair an allowed pane
    /// with an outside run, interaction, task, tab or preview.
    async fn check_selectors(&self, allowed: &Allowed, p: &Value) -> Result<(), ApiError> {
        let deny = || ApiError::new("forbidden", "outside what was shared with you");
        for key in [
            "pane",
            "target",
            "run",
            "interaction",
            "task",
            "check_run",
            "tab",
            "preview",
        ] {
            let Some(v) = p.get(key) else { continue };
            let Some(v) = v.as_str() else {
                return Err(deny());
            };
            let ok = match key {
                "task" => allowed.task_ok(self.task_workspace(v).await.as_deref()),
                "check_run" => {
                    // A check run belongs to a task; resolve it, then judge the task.
                    let r = self
                        .server("task.check.get", json!({"check_run": v}))
                        .await
                        .map_err(|_| deny())?;
                    match s(&r, "task") {
                        Some(t) => allowed.task_ok(self.task_workspace(t).await.as_deref()),
                        None => false,
                    }
                }
                "tab" => match self.tab_of(v).await? {
                    Some((ws, panes)) => allowed.tab_ok(ws.as_deref(), &panes),
                    None => false,
                },
                "preview" => {
                    let r = self
                        .server("preview.get", json!({"preview": v}))
                        .await
                        .map_err(|_| deny())?;
                    match r.pointer("/preview/pane").and_then(|x| x.as_str()) {
                        Some(pane) => matches!(
                            self.locate(&json!({"pane": pane})).await,
                            Ok(Some((id, ws))) if allowed.pane_ok(&id, ws.as_deref())
                        ),
                        None => false,
                    }
                }
                // A run (`agent.turns`) is located like a `target`.
                "run" => matches!(
                    self.locate(&json!({"target": v})).await?,
                    Some((pane, ws)) if allowed.pane_ok(&pane, ws.as_deref())
                ),
                _ => matches!(
                    self.locate(&json!({key: v})).await?,
                    Some((pane, ws)) if allowed.pane_ok(&pane, ws.as_deref())
                ),
            };
            if !ok {
                return Err(deny());
            }
        }
        Ok(())
    }

    /// The workspace a task belongs to (None when unknown or not in a workspace).
    async fn task_workspace(&self, task: &str) -> Option<String> {
        let r = self.server("task.get", json!({"task": task})).await.ok()?;
        r.pointer("/task/workspace")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// A tab's workspace and panes, from the snapshot.
    async fn tab_of(&self, tab: &str) -> Result<Option<(Option<String>, Vec<String>)>, ApiError> {
        let snap = self.server("session.snapshot", json!({})).await?;
        let Some(t) = snap
            .get("tabs")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .find(|t| s(t, "id") == Some(tab) || s(t, "handle") == Some(tab))
        else {
            return Ok(None);
        };
        let tab_id = s(t, "id").unwrap_or(tab);
        let panes: Vec<String> = snap
            .get("panes")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter(|p| s(p, "tab") == Some(tab_id))
            .filter_map(|p| s(p, "id").map(str::to_string))
            .collect();
        Ok(Some((s(t, "workspace").map(str::to_string), panes)))
    }

    /// Panes and tasks visible to a limited device (from the snapshot, as `dashboard.get`).
    async fn visible(&self, allowed: &Allowed) -> Result<(Vec<String>, Vec<String>), ApiError> {
        let raw = self.server("session.snapshot", json!({})).await?;
        let tasks: Vec<String> = raw
            .get("tasks")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter(|t| allowed.task_ok(s(t, "workspace")))
            .filter_map(|t| s(t, "id").map(str::to_string))
            .collect();
        let mut snap = raw;
        Self::filter_snapshot(allowed, &mut snap);
        let panes = snap["panes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| s(p, "id").map(str::to_string))
            .collect();
        Ok((panes, tasks))
    }

    /// Filter list results to the limit: attention items by pane (or task, when they have no
    /// pane), previews by pane. Excluded items are counted, never described.
    fn filter_list(method: &str, r: &mut Value, panes: &[String], tasks: &[String]) {
        let has = |list: &[String], v: Option<&str>| v.is_some_and(|v| list.iter().any(|x| x == v));
        match method {
            "attention.list" => {
                let mut kept_keys = Vec::new();
                let mut excluded = 0;
                if let Some(items) = r.get_mut("items").and_then(|v| v.as_array_mut()) {
                    let before = items.len();
                    items.retain(|it| match s(it, "pane") {
                        Some(pane) => has(panes, Some(pane)),
                        None => has(tasks, s(it, "task")),
                    });
                    excluded = before - items.len();
                    kept_keys = items
                        .iter()
                        .filter_map(|it| it.get("key").cloned())
                        .collect();
                }
                if let Some(c) = r.get_mut("coverage").and_then(|v| v.as_object_mut()) {
                    let prior = c.get("excluded").and_then(|v| v.as_u64()).unwrap_or(0);
                    c.insert("excluded".into(), json!(prior + excluded as u64));
                    c.insert("scope".into(), json!("shared"));
                    c.insert("notes".into(), json!([]));
                }
                if let Some(f) = r.get_mut("five_minute").and_then(|v| v.as_object_mut()) {
                    for k in ["keys", "item_notes"] {
                        if let Some(a) = f.get_mut(k).and_then(|v| v.as_array_mut()) {
                            a.retain(|x| {
                                let key = if k == "keys" { Some(x) } else { x.get("key") };
                                key.is_some_and(|key| kept_keys.contains(key))
                            });
                        }
                    }
                }
            }
            "preview.list" => {
                if let Some(list) = r.get_mut("previews").and_then(|v| v.as_array_mut()) {
                    list.retain(|pv| has(panes, s(pv, "pane")));
                }
            }
            // Screenshots (agent images included) by the pane they belong to. Records without a
            // pane, or of a closed pane, have no live pane to check: dropped.
            "screenshot.list" => {
                let mut dropped = 0;
                let mut kept = 0;
                if let Some(list) = r.get_mut("screenshots").and_then(|v| v.as_array_mut()) {
                    let before = list.len();
                    list.retain(|m| has(panes, s(m, "pane")));
                    kept = list.len();
                    dropped = before - kept;
                }
                if r.get("count").is_some() {
                    r["count"] = json!(kept);
                }
                if let Some(total) = r.get("total").and_then(|v| v.as_u64()) {
                    r["total"] = json!(total.saturating_sub(dropped as u64));
                }
            }
            // Hits of closed (archived) panes have no live pane to check: dropped.
            "search.query" => {
                if let Some(list) = r.get_mut("hits").and_then(|v| v.as_array_mut()) {
                    list.retain(|h| has(panes, s(h, "pane")));
                }
            }
            _ => {}
        }
    }

    /// `worktree.list` runs in a directory: the pane's cwd, the workspace root, or (devices
    /// without a limit only) an explicit `cwd`/`repo`.
    async fn worktree_dir(&self, p: &Value) -> Result<String, ApiError> {
        if let Some(pane) = s(p, "pane") {
            let r = self.server("pane.get", json!({"pane": pane})).await?;
            return s(&r, "cwd")
                .map(str::to_string)
                .ok_or_else(|| ApiError::new("not_found", "pane has no known working directory"));
        }
        if let Some(w) = s(p, "workspace") {
            let snap = self.server("session.snapshot", json!({})).await?;
            return snap
                .get("workspaces")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .find(|x| s(x, "id") == Some(w) || s(x, "handle") == Some(w))
                .and_then(|x| s(x, "root_path"))
                .map(str::to_string)
                .ok_or_else(|| ApiError::new("not_found", "workspace not found"));
        }
        if self.device.limit.is_none()
            && let Some(c) = s(p, "cwd").or(s(p, "repo"))
        {
            return Ok(c.to_string());
        }
        Err(ApiError::invalid("pane or workspace is required"))
    }

    fn filter_snapshot(allowed: &Allowed, snap: &mut Value) {
        // Build an explicit response: only allowlisted keys survive (no previews, tasks, counts).
        let mut out = Map::new();
        for k in [
            "at",
            "at_seq",
            "session",
            "machine",
            "host_name",
            "workspaces",
            "tabs",
            "panes",
            "runs",
            "interactions",
        ] {
            if let Some(v) = snap.get(k) {
                out.insert(k.into(), v.clone());
            }
        }
        out.insert("tasks".into(), json!([]));
        out.insert("notifications_unread".into(), json!(0));
        *snap = Value::Object(out);
        Self::filter_snapshot_lists(allowed, snap);
        if let Some(pane) = &allowed.pane {
            // A single shared pane: only its tab, without the layout of its neighbours.
            if let Some(tabs) = snap.get_mut("tabs").and_then(|v| v.as_array_mut()) {
                let pane_tab = snap_tab_of(pane, tabs);
                tabs.retain(|t| s(t, "id").map(str::to_string) == pane_tab);
                for t in tabs.iter_mut() {
                    if let Some(o) = t.as_object_mut() {
                        o.insert("layout".into(), json!({"Leaf": {"pane": pane}}));
                        o.insert("focused_pane".into(), json!(pane));
                        o.remove("zoomed_pane");
                    }
                }
            }
        }
    }

    fn filter_snapshot_lists(allowed: &Allowed, snap: &mut Value) {
        let panes: Vec<(String, Option<String>)> = snap
            .get("panes")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .map(|p| {
                (
                    s(p, "id").unwrap_or("").to_string(),
                    s(p, "workspace").map(str::to_string),
                )
            })
            .filter(|(id, ws)| allowed.pane_ok(id, ws.as_deref()))
            .collect();
        let pane_ok = |id: Option<&str>| id.is_some_and(|id| panes.iter().any(|(p, _)| p == id));
        let wss: Vec<String> = panes.iter().filter_map(|(_, w)| w.clone()).collect();
        let keep = |arr: &mut Value, f: &dyn Fn(&Value) -> bool| {
            if let Some(a) = arr.as_array_mut() {
                a.retain(|v| f(v));
            }
        };
        if let Some(v) = snap.get_mut("panes") {
            keep(v, &|p| pane_ok(s(p, "id")));
        }
        if let Some(v) = snap.get_mut("workspaces") {
            keep(v, &|w| {
                s(w, "id").is_some_and(|id| wss.iter().any(|x| x == id))
            });
        }
        if let Some(v) = snap.get_mut("tabs") {
            keep(v, &|t| {
                s(t, "workspace").is_some_and(|id| wss.iter().any(|x| x == id))
            });
        }
        for k in ["runs", "interactions"] {
            if let Some(v) = snap.get_mut(k) {
                keep(v, &|r| pane_ok(s(r, "pane")));
            }
        }
        if let Some(v) = snap.get_mut("tasks") {
            *v = json!([]);
        }
    }

    pub async fn dispatch(&self, method: &str, p: Value) -> ApiResult {
        let allowed = Allowed::of(self.device);
        if let Some(a) = &allowed {
            self.check_limit(a, method, &p).await?;
        }
        let mut r = self.dispatch_inner(method, p).await?;
        if let Some(a) = &allowed {
            match method {
                "dashboard.get" => Self::filter_snapshot(a, &mut r),
                "interaction.list" | "notification.list" => {
                    let key = if method == "interaction.list" {
                        "interactions"
                    } else {
                        "notifications"
                    };
                    let snap = self.server("session.snapshot", json!({})).await?;
                    let mut snap = snap;
                    Self::filter_snapshot(a, &mut snap);
                    let panes: Vec<String> = snap["panes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|p| s(p, "id").map(str::to_string))
                        .collect();
                    if let Some(list) = r.get_mut(key).and_then(|v| v.as_array_mut()) {
                        list.retain(|it| {
                            s(it, "pane").is_some_and(|p| panes.iter().any(|x| x == p))
                        });
                    }
                }
                "attention.list" | "preview.list" | "search.query" | "screenshot.list" => {
                    let (panes, tasks) = self.visible(a).await?;
                    Self::filter_list(method, &mut r, &panes, &tasks);
                }
                // The record is fetched by id, so its pane is only known afterwards. Another
                // pane's screenshot answers like a missing one.
                "screenshot.get" => {
                    let (panes, _) = self.visible(a).await?;
                    if !s(&r, "pane").is_some_and(|p| panes.iter().any(|x| x == p)) {
                        return Err(ApiError::new("not_found", "screenshot not found"));
                    }
                }
                _ => {}
            }
        }
        Ok(r)
    }

    async fn dispatch_inner(&self, method: &str, p: Value) -> ApiResult {
        let op_id = s(&p, "op_id").unwrap_or("").to_string();
        match method {
            "ping" => Ok(json!({})),
            "dashboard.get" => {
                let mut snap = self.server("session.snapshot", json!({})).await?;
                normalize(&mut snap);
                let unread = self
                    .server(
                        "notification.list",
                        json!({"unread_only": true, "limit": 200}),
                    )
                    .await
                    .ok()
                    .and_then(|n| {
                        n.get("notifications")
                            .and_then(|a| a.as_array())
                            .map(|a| a.len())
                    })
                    .unwrap_or(0);
                if let Some(its) = snap.get_mut("interactions").and_then(|v| v.as_array_mut()) {
                    for it in its.iter_mut() {
                        self.enrich(it).await;
                    }
                }
                snap["at"] = snap.get("at_seq").cloned().unwrap_or(Value::Null);
                snap["notifications_unread"] = unread.into();
                snap["host_name"] = self.gw.host_name.clone().into();
                Ok(snap)
            }
            "pane.read" => {
                self.server("pane.read", pick(&p, &["pane", "source", "lines"]))
                    .await
            }
            "pane.send_text" => {
                let pane = req(&p, "pane")?;
                let text = s(&p, "text").unwrap_or("");
                let r = self
                    .server(
                        "pane.send_text",
                        json!({"pane": pane, "text": text, "paste": "auto"}),
                    )
                    .await?;
                if p.get("submit").and_then(|v| v.as_bool()) == Some(true) {
                    // Give the TUI a beat to take the paste before Enter (350 ms works well).
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    self.server("pane.send_keys", json!({"pane": pane, "keys": ["Enter"]}))
                        .await?;
                }
                Ok(r)
            }
            "pane.send_keys" => {
                self.server("pane.send_keys", pick(&p, &["pane", "keys"]))
                    .await
            }
            "pane.rename" => {
                self.server("pane.rename", pick(&p, &["pane", "name", "title"]))
                    .await
            }
            "pane.close" => self.server("pane.close", pick(&p, &["pane"])).await,
            "pane.focus" => self.server("pane.focus", pick(&p, &["pane"])).await,
            "agent.prompt" => {
                self.server("agent.prompt", pick(&p, &["target", "text"]))
                    .await
            }
            "agent.interrupt" => self.server("agent.interrupt", pick(&p, &["target"])).await,
            "agent.commands" | "agent.models" => {
                req(&p, "target")?;
                self.server(method, pick(&p, &["target"])).await
            }
            "agent.set_model" => {
                req(&p, "target")?;
                req(&p, "model")?;
                self.server("agent.set_model", pick(&p, &["target", "model", "scope"]))
                    .await
            }
            "agent.harnesses" => self.server("agent.harnesses", json!({})).await,
            "agent.transcript" => {
                // Server pages natively for gateway clients: {turns:[{n, ts, items}], next_before}.
                req(&p, "target")?;
                self.server(
                    "agent.transcript",
                    pick(&p, &["target", "before", "limit", "image"]),
                )
                .await
            }
            "agent.start" => {
                let pane = if let Some(wt) = p.get("worktree") {
                    // A new worktree of the repository at `pane`, `workspace` or `cwd`, opened as
                    // its own workspace; the agent starts in its first pane.
                    let mut params = pick(wt, &["branch", "base", "name"]);
                    req(&params, "branch")?;
                    params["cwd"] = self.worktree_dir(&p).await?.into();
                    params["open"] = true.into();
                    let r = self.server("worktree.create", params).await?;
                    root_pane_id(&r, "worktree.create")?
                } else if let Some(nw) = p.get("new_workspace") {
                    // A new workspace in a folder on the host.
                    let mut params = pick(nw, &["cwd", "name"]);
                    // Path pickers show `~/…`; the host takes the folder as given.
                    let cwd = vk_handoff::expand_home(req(&params, "cwd")?);
                    params["cwd"] = cwd.to_string_lossy().into_owned().into();
                    let r = self.server("workspace.create", params).await?;
                    root_pane_id(&r, "workspace.create")?
                } else {
                    match s(&p, "pane") {
                        Some(pane) => pane.to_string(),
                        None => {
                            let t = self
                                .server("tab.create", pick(&p, &["workspace", "cwd"]))
                                .await?;
                            root_pane_id(&t, "tab.create")?
                        }
                    }
                };
                let mut params = pick(&p, &["harness", "prompt", "name"]);
                params["pane"] = pane.clone().into();
                let mut r = self.server("agent.start", params).await?;
                r["pane"] = pane.into();
                Ok(r)
            }
            "tab.create" => {
                let mut r = self
                    .server("tab.create", pick(&p, &["workspace", "cwd", "title"]))
                    .await?;
                // Spec 16 §7.4 shape: {tab, pane} (the server calls it root_pane).
                if let Some(rp) = r.get("root_pane").cloned() {
                    r["pane"] = rp;
                }
                Ok(r)
            }
            "interaction.list" => {
                let mut r = self
                    .server("interaction.list", pick(&p, &["status", "kind"]))
                    .await?;
                normalize(&mut r);
                if let Some(its) = r.get_mut("interactions").and_then(|v| v.as_array_mut()) {
                    for it in its.iter_mut() {
                        self.enrich(it).await;
                    }
                }
                Ok(r)
            }
            "interaction.get" => {
                Ok(json!({"interaction": self.interaction(req(&p, "interaction")?).await?}))
            }
            "interaction.answer" => {
                let id = req(&p, "interaction")?;
                if p.get("decision").is_none()
                    && p.get("choices").is_none()
                    && p.get("text").is_none()
                {
                    return Err(ApiError::invalid("decision, choices or text is required"));
                }
                self.answer_one(id, required_rev(&p)?, &p, &op_id, None)
                    .await
            }
            "interaction.answer_batch" => {
                let decision = req(&p, "decision")?;
                if !matches!(decision, "allow" | "deny") {
                    return Err(ApiError::invalid("batch decisions are allow or deny"));
                }
                let items = p
                    .get("items")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                if items.is_empty() || items.len() > 50 {
                    return Err(ApiError::invalid("1–50 items"));
                }
                // Re-validate the whole group before answering anything.
                let mut group: Option<String> = None;
                for item in &items {
                    let id = req(item, "interaction")?;
                    let it = self.interaction(id).await?;
                    let fp = fingerprint(&it);
                    let rev_ok = Some(required_rev(item)?)
                        == it.get("decision_rev").and_then(|v| v.as_u64());
                    if !batch_eligible(&it)
                        || fp.is_none()
                        || !rev_ok
                        || (group.is_some() && group != fp)
                    {
                        return Err(ApiError {
                            kind: "stale".into(),
                            message: "batch no longer valid; refresh".into(),
                            details: json!({"interaction": id}),
                        });
                    }
                    group = fp;
                }
                let mut results = Vec::new();
                for item in &items {
                    let id = req(item, "interaction")?;
                    let r = self
                        .answer_one(
                            id,
                            required_rev(item)?,
                            &json!({"decision": decision}),
                            &op_id,
                            group.as_deref(),
                        )
                        .await;
                    results.push(match r {
                        Ok(v) => json!({"interaction": id, "ok": true, "result": v}),
                        Err(e) => json!({"interaction": id, "ok": false, "error": e.to_json()}),
                    });
                }
                Ok(json!({"results": results}))
            }
            "git.status" => self.server("git.status", pick(&p, &["pane"])).await,
            "git.diff" => {
                self.server(
                    "git.diff",
                    pick(&p, &["pane", "file", "staged", "base", "range"]),
                )
                .await
            }
            "git.log" => {
                self.server("git.log", pick(&p, &["pane", "base", "limit"]))
                    .await
            }
            "fs.list" | "fs.read" => {
                req(&p, "pane")?;
                self.server(method, pick(&p, &["pane", "path", "as"])).await
            }
            "attention.list" => {
                let mut r = self
                    .server("attention.list", pick(&p, &["budget_ms", "effort"]))
                    .await?;
                normalize(&mut r);
                Ok(r)
            }
            "attention.update" => {
                self.server(
                    "attention.update",
                    pick_nullable(&p, &["key", "seen", "snooze_until_ms", "pin", "item_rev"]),
                )
                .await
            }
            "task.review.get" | "task.check.list" => {
                self.server(method, pick(&p, &["task", "subject"])).await
            }
            "task.review.candidates" => self.server(method, pick(&p, &["task"])).await,
            "task.review.diff" => {
                self.server(method, pick(&p, &["task", "subject", "path", "max_bytes"]))
                    .await
            }
            "task.check.get" => self.server(method, pick(&p, &["check_run"])).await,
            "task.check.run" => {
                let mut params = pick(
                    &p,
                    &["task", "subject", "check", "definition_digest", "authorize"],
                );
                params["idempotency_key"] = format!("gw:{}:{op_id}", self.device.id).into();
                self.server(method, params).await
            }
            "preview.list" => {
                self.server(
                    method,
                    pick(&p, &["machine", "status", "all", "task", "pane"]),
                )
                .await
            }
            "preview.get" | "preview.url" | "preview.promote" | "preview.forget" => {
                req(&p, "preview")?;
                self.server(method, pick(&p, &["preview", "machine"])).await
            }
            "preview.status" => self.server(method, json!({})).await,
            "screenshot.list" => {
                let mut params = pick(
                    &p,
                    &[
                        "task",
                        "pane",
                        "workspace",
                        "environment",
                        "since",
                        "since_ms",
                        "limit",
                    ],
                );
                // A limited device asks the server for its own pane or workspace, so `limit`
                // counts records it may see (`dispatch` filters the result again).
                if let Some(a) = Allowed::of(self.device)
                    && params.get("pane").is_none()
                {
                    if let Some(pane) = &a.pane {
                        params["pane"] = pane.clone().into();
                    } else if let Some(w) = &a.workspace
                        && params.get("workspace").is_none()
                    {
                        params["workspace"] = w.clone().into();
                    }
                }
                self.server(method, params).await
            }
            "screenshot.get" => {
                req(&p, "id")?;
                self.server(method, pick(&p, &["id", "inline"])).await
            }
            "preview.open" => {
                self.server(
                    method,
                    pick(
                        &p,
                        &[
                            "preview", "url", "machine", "window", "split", "focus", "pane",
                        ],
                    ),
                )
                .await
            }
            "worktree.list" => {
                let cwd = self.worktree_dir(&p).await?;
                self.server(method, json!({"cwd": cwd})).await
            }
            // `{branch, base?, name?, open?}` in the repository at `pane`, `workspace` or `cwd`.
            "worktree.create" => {
                req(&p, "branch")?;
                let mut params = pick(&p, &["branch", "base", "name", "open"]);
                params["cwd"] = self.worktree_dir(&p).await?.into();
                self.server(method, params).await
            }
            // A workspace in a host folder (never a `command` or `layout`).
            "workspace.create" => {
                req(&p, "cwd")?;
                self.server(method, pick(&p, &["cwd", "name"])).await
            }
            "agent.turns" => {
                req(&p, "run")?;
                self.server(method, pick(&p, &["run", "after_seq", "limit"]))
                    .await
            }
            "assistant.status" => self.server(method, json!({})).await,
            "assistant.get" | "assistant.cancel" => {
                req(&p, "request")?;
                self.server(method, pick(&p, &["request"])).await
            }
            "assistant.generate" => {
                let op = req(&p, "operation")?;
                if !APP_ASSIST_OPS.contains(&op) {
                    return Err(ApiError::new(
                        "forbidden",
                        format!(
                            "the {op} operation is not available to apps (only {})",
                            APP_ASSIST_OPS.join(", ")
                        ),
                    ));
                }
                // No profile, remote sources or raw `inputs`: the host's defaults and consent
                // decide what is sent where.
                let mut params = pick(
                    &p,
                    &[
                        "operation",
                        "workspace",
                        "pane",
                        "run",
                        "interaction",
                        "turns",
                        "include_screen",
                        "priority",
                    ],
                );
                params["idempotency_key"] = format!("gw:{}:{op_id}", self.device.id).into();
                self.server(method, params).await
            }
            "assistant.confirm" => {
                req(&p, "request")?;
                req(&p, "preview_digest")?;
                self.server(method, pick(&p, &["request", "preview_digest"]))
                    .await
            }
            "desk.search" => {
                req(&p, "text")?;
                self.server(
                    method,
                    pick(
                        &p,
                        &[
                            "text", "repo", "harness", "since", "until", "session", "limit",
                            "sort", "fresh",
                        ],
                    ),
                )
                .await
                .map(|mut r| {
                    // Transcript excerpts leave the host: secrets are redacted (the server does
                    // the same for `search.query` hits of remote clients).
                    for h in r
                        .get_mut("hits")
                        .and_then(|v| v.as_array_mut())
                        .into_iter()
                        .flatten()
                    {
                        let red = h
                            .get("snippet")
                            .and_then(|v| v.as_str())
                            .map(|t| vk_redact::redact(t).to_string());
                        if let Some(red) = red {
                            h["snippet"] = red.into();
                        }
                    }
                    r
                })
            }
            "search.query" => {
                req(&p, "q")?;
                let mut params = pick(
                    &p,
                    &[
                        "q",
                        "pane",
                        "workspace",
                        "sources",
                        "since",
                        "limit",
                        "regex",
                        "context",
                    ],
                );
                // A limited device searches only what was shared (results are filtered again).
                if let Some(a) = Allowed::of(self.device) {
                    match (&a.pane, &a.workspace) {
                        (Some(pane), _) => params["pane"] = pane.clone().into(),
                        (None, Some(w)) => params["workspace"] = w.clone().into(),
                        (None, None) => {}
                    }
                }
                self.server(method, params).await
            }
            "sandbox.list" | "sandbox.status" | "browser.list" | "browser.status" => {
                self.server(method, json!({})).await
            }
            "browser.attach_screencast" => {
                let session = req(&p, "session")?;
                crate::screencast::attach(self.gw, &self.actor(), &self.device.id, session).await
            }
            "browser.detach_screencast" => {
                crate::screencast::detach(self.gw, &self.device.id, req(&p, "session")?).await
            }
            "browser.screencast_frame" => {
                req(&p, "session")?;
                crate::screencast::frame(
                    self.gw,
                    &self.device.id,
                    pick(&p, &["session", "after_seq"]),
                )
                .await
            }
            "browser.take_over" | "browser.release" => {
                let session = req(&p, "session")?;
                self.still_authorized()?;
                crate::screencast::control(
                    self.gw,
                    &self.actor(),
                    &self.device.id,
                    session,
                    method == "browser.take_over",
                )
                .await
            }
            "browser.click" | "browser.type" | "browser.press" | "browser.navigate" => {
                // Input goes to a session this device took over, so the agent is paused meanwhile.
                let session = req(&p, "session")?;
                if self.gw.screencasts.controller(session).as_deref()
                    != Some(self.device.id.as_str())
                {
                    return Err(ApiError::new(
                        "conflict",
                        "take over this browser session first (browser.take_over)",
                    ));
                }
                let keys: &[&str] = match method {
                    "browser.click" => &[
                        "session",
                        "selector",
                        "text",
                        "x",
                        "y",
                        "click_count",
                        "timeout_ms",
                    ],
                    "browser.type" => &[
                        "session",
                        "text",
                        "selector",
                        "clear",
                        "submit",
                        "timeout_ms",
                    ],
                    "browser.press" => &["session", "key"],
                    _ => &["session", "url", "path", "wait", "timeout_ms"],
                };
                self.server(method, pick(&p, keys)).await
            }
            "fs.browse" => self.server(method, pick(&p, &["path", "prefix"])).await,
            "repo.candidates" => {
                req(&p, "origin")?;
                self.server(method, pick(&p, &["origin"])).await
            }
            "tab.rename" => {
                req(&p, "tab")?;
                self.server(method, pick(&p, &["tab", "title"])).await
            }
            "tab.close" | "tab.focus" => {
                req(&p, "tab")?;
                self.server(method, pick(&p, &["tab"])).await
            }
            "attachment.put" => {
                let data = req(&p, "data_b64")?;
                if data.len() > 11 * 1024 * 1024 {
                    return Err(ApiError::new(
                        "too_large",
                        "attachments are limited to 8 MiB",
                    ));
                }
                self.server("image.upload", pick(&p, &["data_b64", "name", "mime"]))
                    .await
            }
            "notification.list" => {
                self.server("notification.list", pick(&p, &["unread_only", "limit"]))
                    .await
            }
            "notification.read" => {
                self.server("notification.read", pick(&p, &["notification", "all"]))
                    .await
            }
            "prefs.get" => {
                let host = self.gw.state.host_prefs().unwrap_or_default();
                Ok(json!({"device": self.device.prefs, "host": host}))
            }
            "prefs.set" => {
                if let Some(dp) = p.get("device") {
                    // Partial update: merge the given fields over the stored prefs.
                    let mut merged = serde_json::to_value(&self.device.prefs).unwrap_or_default();
                    if let (Some(m), Some(patch)) = (merged.as_object_mut(), dp.as_object()) {
                        for (k, v) in patch {
                            m.insert(k.clone(), v.clone());
                        }
                    }
                    let prefs: crate::state::DevicePrefs = serde_json::from_value(merged)
                        .map_err(|e| ApiError::invalid(e.to_string()))?;
                    self.gw
                        .update_device(&self.device.id, |d| d.prefs = prefs.clone())
                        .map_err(internal)?;
                }
                if let Some(h) = p.get("host") {
                    // Host-wide DND silences every device, so it needs full scope.
                    if self.device.scope < Scope::Full {
                        return Err(ApiError::new(
                            "forbidden",
                            "host-wide settings need full scope",
                        ));
                    }
                    let mut hp = self.gw.state.host_prefs().unwrap_or_default();
                    if let Some(u) = h.get("dnd_until").and_then(|v| v.as_u64()) {
                        hp.dnd_until = u;
                    }
                    self.gw.state.save_host_prefs(&hp).map_err(internal)?;
                }
                Ok(json!({}))
            }
            "relay.ticket" => {
                let (ticket, exp) = self.gw.device_ticket(self.device);
                Ok(json!({"ticket": ticket, "exp": exp}))
            }
            "push.subscribe" => {
                let sub: crate::push::Subscription =
                    serde_json::from_value(p.get("subscription").cloned().unwrap_or_default())
                        .map_err(|e| ApiError::invalid(e.to_string()))?;
                if crate::push::endpoint_allowed(&sub.endpoint, &self.gw.push.allowed).is_none() {
                    return Err(ApiError::invalid(
                        "push endpoint is not an allowed push service",
                    ));
                }
                let vapid = req(&p, "vapid_private")?.to_string();
                // The app's service worker closes notifications on a `clear` push (notify.rs).
                let supports_clear =
                    p.get("supports_clear").and_then(|v| v.as_bool()) == Some(true);
                crate::push::vapid_public(
                    &vk_e2e::b64::decode(&vapid).map_err(|_| ApiError::invalid("vapid_private"))?,
                )
                .map_err(|_| ApiError::invalid("vapid_private"))?;
                self.gw
                    .update_device(&self.device.id, |d| {
                        d.vapid_private = Some(vapid.clone());
                        d.supports_clear = supports_clear;
                        d.push.retain(|s| s.endpoint != sub.endpoint);
                        d.push.push(sub.clone());
                        if d.push.len() > 3 {
                            d.push.remove(0);
                        }
                    })
                    .map_err(internal)?;
                Ok(json!({}))
            }
            "push.unsubscribe" => {
                let endpoint = s(&p, "endpoint").map(str::to_string);
                self.gw
                    .update_device(&self.device.id, |d| match &endpoint {
                        Some(e) => d.push.retain(|s| &s.endpoint != e),
                        None => d.push.clear(),
                    })
                    .map_err(internal)?;
                Ok(json!({}))
            }
            "push.test" => {
                let n = self.gw.push_to(&self.device.id, &json!({"title": "Vibeke", "body": format!("Test from {}", self.gw.host_name), "tag": format!("vibeke:{}:test", self.gw.keys.host_id())}), "normal").await;
                Ok(json!({"sent": n}))
            }
            "devices.list" => devices_list_as(self.gw, self.device),
            "devices.revoke" => devices_revoke_as(self.gw, self.device, &p).await,
            "stt.transcribe" => crate::stt::transcribe(self.gw, &p).await,
            "share.create" => self.share_create(&p),
            m if m.starts_with("peer.") || m == "share.list" || m == "share.revoke" => {
                crate::peers::dispatch(self.gw, self.device, m, &p).await
            }
            // Outgoing jobs live in the server; this gateway's worker runs them.
            "handoff.send" => {
                req(&p, "peer")?;
                self.server(method, pick(&p, &["pane", "peer", "interrupt"]))
                    .await
            }
            "handoff.cancel" => {
                req(&p, "id")?;
                self.server(method, pick(&p, &["id"])).await
            }
            "handoff.jobs" | "handoff.peers" => self.server(method, json!({})).await,
            // Approved calls (09 §3.2): only the approval requests and standing grants, never
            // the elevation tokens and revocations `auth.list` also returns.
            "auth.list" => {
                let r = self.server(method, json!({})).await?;
                Ok(json!({
                    "approvals": r.get("approvals").cloned().unwrap_or_else(|| json!([])),
                    "grants": r.get("grants").cloned().unwrap_or_else(|| json!([])),
                }))
            }
            "auth.approve.decide" => {
                req(&p, "request")?;
                req(&p, "decision")?;
                self.server(method, pick(&p, &["request", "decision"]))
                    .await
            }
            // Delivery from another host.
            "handoff.offer" | "handoff.status" | "handoff.write" | "handoff.commit"
            | "handoff.discard" => {
                crate::handoff_peer::dispatch(self.gw, self.device, method, &p).await
            }
            // This host's own incoming handoffs (full-scope devices; the server decides).
            m if m.starts_with("handoff.") => {
                crate::handoff::dispatch(self.gw, self.device, m, &p).await
            }
            _ => Err(ApiError::new(
                "method_not_found",
                format!("unknown method {method}"),
            )),
        }
    }
}

/// `devices.list`, for the app API and the server bridge (`gateway.call`): `this` marks `device`.
pub fn devices_list_as(gw: &Arc<Gateway>, device: &Device) -> ApiResult {
    // Another process (`vibeke-gateway revoke`, a claim) may have changed the registry.
    let _ = gw.reload_devices();
    let me = &device.id;
    let list: Vec<Value> = gw
        .devices()
        .iter()
        .map(|d| json!({"id": d.id, "name": d.name, "platform": d.platform, "scope": d.scope, "paired_at": d.paired_at, "fingerprint": d.fingerprint(), "push": !d.push.is_empty(), "this": &d.id == me, "kind": d.kind, "expires_at": d.expires_at, "limit": d.limit, "peer": d.peer}))
        .collect();
    Ok(json!({"devices": list}))
}

/// `devices.revoke {device}`, for the app API and the server bridge. A device never revokes
/// itself here.
pub async fn devices_revoke_as(gw: &Arc<Gateway>, device: &Device, p: &Value) -> ApiResult {
    let id = req(p, "device")?;
    if id == device.id {
        return Err(ApiError::invalid(
            "a device cannot revoke itself here; use Forget host",
        ));
    }
    gw.revoke(id).await.map_err(internal)?;
    Ok(json!({}))
}

/// `share.create` for the server bridge (`gateway.call`): the same code as the app API.
pub fn share_create_as(gw: &Arc<Gateway>, device: &Device, p: &Value) -> ApiResult {
    Call { gw, device }.share_create(p)
}

impl Call<'_> {
    fn share_create(&self, p: &Value) -> ApiResult {
        let kind = s(p, "kind").unwrap_or("share");
        let (scope, default_ttl) = match kind {
            "share" => (
                match s(p, "scope").unwrap_or("view") {
                    "view" => Scope::View,
                    "approve" => Scope::Approve,
                    // Control: type into and prompt the shared pane(s), still limited to them.
                    "full" | "control" => Scope::Full,
                    _ => return Err(ApiError::invalid("a share is view, approve or full")),
                },
                2 * 3600,
            ),
            "handoff" => (Scope::Full, 24 * 3600),
            _ => return Err(ApiError::invalid("kind is share or handoff")),
        };
        let ttl_s = p
            .get("ttl_s")
            .and_then(|v| v.as_u64())
            .unwrap_or(default_ttl)
            .clamp(60, 7 * 24 * 3600);
        let limit = match (s(p, "workspace"), s(p, "pane")) {
            (None, None) if kind == "share" => {
                return Err(ApiError::invalid("share a workspace or a pane"));
            }
            (None, None) => None,
            (w, pane) => Some(crate::state::Limit {
                workspace: w.map(str::to_string),
                pane: pane.map(str::to_string),
            }),
        };
        let relay = self
            .gw
            .cfg
            .relay
            .clone()
            .ok_or_else(|| ApiError::unavailable("no relay configured"))?;
        let until = now_s() + ttl_s;
        let label = s(p, "label").or(s(p, "name")).map(str::to_string);
        let spec = crate::state::ShareSpec {
            kind: kind.into(),
            ttl_s,
            until,
            limit,
            label,
            owner: None,
        };
        let (pairing, link) = crate::pair::create_with(
            &self.gw.state,
            &relay,
            &self.gw.host_name,
            scope,
            true,
            Duration::from_secs(15 * 60),
            Some(spec),
        )
        .map_err(internal)?;
        let app = self.gw.cfg.app_url.clone().ok_or_else(|| {
            ApiError::unavailable(
                "no app origin configured on this host (vibeke-gateway run --app-url …)",
            )
        })?;
        self.gw.state.audit(&json!({"ts": now_s(), "event": "share.created", "by": self.device.id, "kind": kind, "pid": pairing.pid}));
        Ok(
            json!({"link": link.to_url(&app), "pid": pairing.pid, "open_by": pairing.exp, "expires_at": until, "expires_after_s": ttl_s}),
        )
    }
}

fn stale(it: &Value) -> ApiError {
    ApiError {
        kind: "stale".into(),
        message: "interaction changed; refresh and decide again".into(),
        details: json!({"interaction": it}),
    }
}

/// The first pane's id of a `tab.create`, `workspace.create` or `worktree.create {open}` result.
fn root_pane_id(r: &Value, method: &str) -> Result<String, ApiError> {
    r.get("root_pane")
        .and_then(|r| r.get("id").or(Some(r)))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| ApiError::new("internal", format!("{method} returned no root pane")))
}

/// What a sandbox boundary request (`sandbox.request`, sandbox_boundary.rs) asks for, from the
/// interaction the server opened: `{kind: push|copy_out, pane, remote?, branch?, path?}`.
pub fn boundary_of(it: &Value) -> Option<Value> {
    if it.pointer("/action/tool").and_then(|t| t.as_str()) != Some("boundary") {
        return None;
    }
    // `native_ref` is `boundary:<kind>:<box>`.
    let kind = s(it, "native_ref")?
        .strip_prefix("boundary:")?
        .split(':')
        .next()?
        .to_string();
    let mut b = json!({"kind": kind, "pane": it.get("pane").cloned().unwrap_or(Value::Null)});
    match kind.as_str() {
        "push" => {
            // The summary is `git push <remote> <branch> (from the host, hooks off)`.
            let summary = it
                .pointer("/action/summary")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut w = summary
                .strip_prefix("git push ")
                .unwrap_or("")
                .split_whitespace();
            if let (Some(remote), Some(branch)) = (w.next(), w.next()) {
                b["remote"] = remote.into();
                b["branch"] = branch.into();
            }
        }
        "copy_out" => {
            if let Some(path) = it.pointer("/action/paths/0") {
                b["path"] = path.clone();
            }
        }
        _ => {}
    }
    Some(b)
}

fn required_rev(p: &Value) -> Result<u64, ApiError> {
    p.get("decision_rev")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| ApiError::invalid("decision_rev is required (the revision you decided on)"))
}

/// The tab whose layout contains `pane` (searched textually in the layout JSON).
fn snap_tab_of(pane: &str, tabs: &[Value]) -> Option<String> {
    tabs.iter()
        .find(|t| {
            t.get("layout")
                .is_some_and(|l| l.to_string().contains(&format!("\"{pane}\"")))
        })
        .and_then(|t| s(t, "id").map(str::to_string))
}

pub fn internal(e: anyhow::Error) -> ApiError {
    ApiError::new("internal", e.to_string())
}

/// Nearest ancestor containing `.git` (cheap, no subprocess).
pub fn repo_root(cwd: &str) -> String {
    let mut p = std::path::Path::new(cwd);
    loop {
        if p.join(".git").exists() {
            return p.display().to_string();
        }
        match p.parent() {
            Some(parent) => p = parent,
            None => return cwd.to_string(),
        }
    }
}

/// Per-device operation cache (spec 16 §7.3).
type OpKey = (String, String);

enum Slot {
    Running(tokio::sync::watch::Receiver<Option<ApiResult>>),
    Done(ApiResult),
}

/// Per-device operation ids (spec 16 §7.3): the first request with an `op_id` reserves it; a
/// concurrent or later duplicate (same method + params) gets the same result without running
/// again; reuse with different params is refused.
#[derive(Default)]
pub struct Ops {
    map: std::sync::Mutex<std::collections::HashMap<OpKey, (blake3::Hash, Slot, Instant)>>,
}

pub enum OpCheck<'a> {
    New(OpGuard<'a>),
    Wait(tokio::sync::watch::Receiver<Option<ApiResult>>),
    Done(ApiResult),
    Conflict,
}

pub struct OpGuard<'a> {
    ops: &'a Ops,
    key: OpKey,
    tx: tokio::sync::watch::Sender<Option<ApiResult>>,
    done: bool,
}

impl OpGuard<'_> {
    pub fn complete(mut self, r: &ApiResult) {
        let mut m = self.ops.map.lock().unwrap();
        if let Some(e) = m.get_mut(&self.key) {
            e.1 = Slot::Done(r.clone());
            e.2 = Instant::now();
        }
        drop(m);
        let _ = self.tx.send(Some(r.clone()));
        self.done = true;
    }
}

impl Drop for OpGuard<'_> {
    fn drop(&mut self) {
        if !self.done {
            // Cancelled mid-flight: the call may already have reached the server, so keep a
            // tombstone. A retry with this op_id learns the outcome is unknown instead of running
            // again (spec 16 §1.7).
            let unknown: ApiResult = Err(ApiError::new(
                "outcome_unknown",
                "the earlier attempt was interrupted; refresh to see whether it happened",
            ));
            if let Some(e) = self.ops.map.lock().unwrap().get_mut(&self.key) {
                e.1 = Slot::Done(unknown.clone());
                e.2 = Instant::now();
            }
            let _ = self.tx.send(Some(unknown));
        }
    }
}

impl Ops {
    pub fn reserve(&self, device: &str, op_id: &str, method: &str, params: &Value) -> OpCheck<'_> {
        let h = hash_params(method, params);
        let key: OpKey = (device.into(), op_id.into());
        let mut m = self.map.lock().unwrap();
        m.retain(|_, (_, slot, t)| {
            matches!(slot, Slot::Running(_)) || t.elapsed() < Duration::from_secs(600)
        });
        match m.get(&key) {
            Some((eh, _, _)) if *eh != h => OpCheck::Conflict,
            Some((_, Slot::Done(r), _)) => OpCheck::Done(r.clone()),
            Some((_, Slot::Running(rx), _)) => OpCheck::Wait(rx.clone()),
            None => {
                let (tx, rx) = tokio::sync::watch::channel(None);
                m.insert(key.clone(), (h, Slot::Running(rx), Instant::now()));
                OpCheck::New(OpGuard {
                    ops: self,
                    key,
                    tx,
                    done: false,
                })
            }
        }
    }
}

fn hash_params(method: &str, p: &Value) -> blake3::Hash {
    let mut p = p.clone();
    if let Some(m) = p.as_object_mut() {
        m.remove("op_id");
    }
    blake3::hash(format!("{method}\u{0}{p}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_enums() {
        let mut v = json!({"kind": "PlanReview", "status": "Open", "title": "Approval", "action": {"risk": "High"},
                           "execution": {"value": "RateLimited"}, "delivery": "DeliveryUnknown"});
        normalize(&mut v);
        assert_eq!(v["kind"], "plan_review");
        assert_eq!(v["status"], "open");
        assert_eq!(v["title"], "Approval");
        assert_eq!(v["action"]["risk"], "high");
        assert_eq!(v["execution"]["value"], "rate_limited");
        assert_eq!(v["delivery"], "delivery_unknown");
    }

    #[test]
    fn batch_rules() {
        let it = json!({"kind": "approval", "status": "open", "answerable": true, "harness": "codex", "repo_root": "/r",
                        "action": {"tool": "Bash", "command": "pnpm  test", "risk": "low"}});
        assert!(batch_eligible(&it));
        let mut other = it.clone();
        other["action"]["command"] = "pnpm  test".into();
        assert_eq!(fingerprint(&it), fingerprint(&other));
        // Whitespace is meaningful to a shell: never merge these (Codex review P1-9).
        let mut a1 = it.clone();
        a1["action"]["command"] = "echo harmless rm notes.txt".into();
        let mut a2 = it.clone();
        a2["action"]["command"] = "echo harmless\nrm notes.txt".into();
        assert_ne!(fingerprint(&a1), fingerprint(&a2));
        other["repo_root"] = "/s".into();
        assert_ne!(fingerprint(&it), fingerprint(&other));
        let mut high = it.clone();
        high["action"]["risk"] = "high".into();
        assert!(!batch_eligible(&high));
        let mut unknown = it.clone();
        unknown["action"]["risk"] = "unknown".into();
        assert!(!batch_eligible(&unknown));
    }

    #[test]
    fn scopes() {
        assert_eq!(required_scope("dashboard.get"), Some(Scope::View));
        assert_eq!(required_scope("interaction.answer"), Some(Scope::Approve));
        assert_eq!(required_scope("agent.prompt"), Some(Scope::Full));
        assert_eq!(required_scope("agent.commands"), Some(Scope::View));
        assert_eq!(required_scope("agent.models"), Some(Scope::View));
        assert_eq!(required_scope("agent.set_model"), Some(Scope::Full));
        assert!(is_mutating("agent.set_model"));
        assert!(!is_mutating("agent.models") && !is_mutating("agent.commands"));
        assert_eq!(required_scope("pane.send_keys"), Some(Scope::Full));
        assert_eq!(required_scope("server.stop"), None);
        assert!(is_mutating("interaction.answer"));
        assert!(!is_mutating("pane.read"));
        // Every device kind may renew its relay ticket; it changes nothing.
        assert_eq!(required_scope("relay.ticket"), Some(Scope::View));
        assert!(!is_mutating("relay.ticket"));
        for kind in ["device", "share", "peer"] {
            assert!(kind_allows(kind, "relay.ticket"), "{kind}");
        }
    }

    #[test]
    fn ops_bind_params() {
        let ops = Ops::default();
        let p = json!({"op_id": "1", "x": 1});
        let OpCheck::New(g) = ops.reserve("d", "1", "m", &p) else {
            panic!("expected new")
        };
        // A concurrent duplicate waits instead of running.
        assert!(matches!(ops.reserve("d", "1", "m", &p), OpCheck::Wait(_)));
        g.complete(&Ok(json!({"ok": true})));
        assert!(matches!(
            ops.reserve("d", "1", "m", &p),
            OpCheck::Done(Ok(_))
        ));
        assert!(matches!(
            ops.reserve("d", "1", "m", &json!({"op_id": "1", "x": 2})),
            OpCheck::Conflict
        ));
        // Same params under another method is a different operation.
        assert!(matches!(
            ops.reserve("d", "1", "other", &p),
            OpCheck::Conflict
        ));
        // A dropped (interrupted) reservation becomes "outcome unknown", never a second run.
        drop(ops.reserve("d", "2", "m", &p));
        assert!(
            matches!(ops.reserve("d", "2", "m", &p), OpCheck::Done(Err(e)) if e.kind == "outcome_unknown")
        );
    }
}

#[cfg(test)]
mod share_tests {
    use super::*;

    #[test]
    fn limits_filter_snapshot_and_events() {
        let a = Allowed {
            workspace: Some("w1".into()),
            pane: None,
        };
        let mut snap = json!({
            "workspaces": [{"id": "w1"}, {"id": "w2"}],
            "tabs": [{"id": "t1", "workspace": "w1"}, {"id": "t2", "workspace": "w2"}],
            "panes": [{"id": "p1", "workspace": "w1"}, {"id": "p2", "workspace": "w2"}],
            "runs": [{"id": "r1", "pane": "p1"}, {"id": "r2", "pane": "p2"}],
            "interactions": [{"id": "i1", "pane": "p1"}, {"id": "i2", "pane": "p2"}],
            "tasks": [{"id": "x"}]
        });
        Call::filter_snapshot(&a, &mut snap);
        assert_eq!(snap["workspaces"], json!([{"id": "w1"}]));
        assert_eq!(snap["panes"].as_array().unwrap().len(), 1);
        assert_eq!(snap["runs"], json!([{"id": "r1", "pane": "p1"}]));
        assert_eq!(snap["interactions"], json!([{"id": "i1", "pane": "p1"}]));
        assert_eq!(snap["tasks"], json!([]));
        assert!(a.subject_ok(&json!({"pane": "p1", "workspace": "w1"})));
        assert!(!a.subject_ok(&json!({"pane": "p2", "workspace": "w2"})));
        assert!(
            !a.subject_ok(&json!({"interaction": "i9"})),
            "events without a scope subject are hidden"
        );

        let only_pane = Allowed {
            workspace: None,
            pane: Some("p2".into()),
        };
        assert!(only_pane.pane_ok("p2", Some("w2")));
        assert!(!only_pane.pane_ok("p1", Some("w2")));
    }

    #[test]
    fn device_kinds() {
        // The retired courier kind gets nothing.
        assert!(!kind_allows("handoff", "handoff.write"));
        assert!(!kind_allows("handoff", "hello"));
        assert!(!kind_allows("share", "devices.revoke"));
        assert!(!kind_allows("share", "share.create"));
        assert!(!kind_allows("share", "handoff.status"));
        assert!(kind_allows("share", "pane.read"));
        for m in [
            "handoff.export",
            "handoff.read",
            "handoff.begin",
            "handoff.finish",
        ] {
            assert_eq!(required_scope(m), None, "{m}");
        }
        // Peers (another host) only deliver handoffs.
        for m in [
            "hello",
            "ping",
            "handoff.offer",
            "handoff.status",
            "handoff.write",
            "handoff.commit",
            "handoff.discard",
        ] {
            assert!(kind_allows("peer", m), "{m}");
        }
        for m in [
            "devices.list",
            "dashboard.get",
            "events.subscribe",
            "handoff.accept",
            "handoff.send",
            "peer.invite",
            "share.list",
            "pane.send_text",
        ] {
            assert!(!kind_allows("peer", m), "{m}");
        }
        for m in [
            "peer.invite",
            "peer.redeem",
            "peer.list",
            "peer.remove",
            "share.list",
            "share.revoke",
        ] {
            assert_eq!(required_scope(m), Some(Scope::Full), "{m}");
            assert!(
                !kind_allows("share", m) && !kind_allows("handoff", m),
                "{m}"
            );
            assert!(kind_allows("device", m), "{m}");
        }
        // Outgoing jobs: the owner's apps only.
        for m in [
            "handoff.send",
            "handoff.jobs",
            "handoff.cancel",
            "handoff.peers",
        ] {
            assert_eq!(required_scope(m), Some(Scope::Full), "{m}");
            assert!(
                !kind_allows("peer", m) && !kind_allows("handoff", m) && !kind_allows("share", m),
                "{m}"
            );
            assert!(kind_allows("device", m), "{m}");
        }
        assert!(is_mutating("handoff.send") && is_mutating("handoff.cancel"));
        assert!(is_mutating("handoff.offer") && is_mutating("handoff.commit"));
        assert!(!is_mutating("handoff.jobs") && !is_mutating("handoff.status"));
        assert!(is_mutating("peer.redeem") && is_mutating("share.revoke"));
        // Approved calls: the owner's full-scope apps decide; share devices never see them.
        for m in ["auth.list", "auth.approve.decide"] {
            assert_eq!(required_scope(m), Some(Scope::Full), "{m}");
            assert!(!kind_allows("share", m) && !kind_allows("peer", m), "{m}");
            assert!(kind_allows("device", m), "{m}");
        }
        assert!(is_mutating("auth.approve.decide") && !is_mutating("auth.list"));
        assert_eq!(required_scope("auth.elevate.decide"), None);
        assert!(!is_mutating("peer.list") && !is_mutating("share.list"));
        assert!(!kind_allows("from-the-future", "ping"));
        // Push registration: own devices only.
        for m in ["push.subscribe", "push.unsubscribe", "push.test"] {
            assert!(kind_allows("device", m), "{m}");
            assert!(!kind_allows("share", m), "{m}");
            assert!(!kind_allows("peer", m), "{m}");
            assert!(!kind_allows("handoff", m), "{m}");
        }
    }
}

#[cfg(test)]
mod workspace_tests {
    //! Workspace-view passthroughs: scope table, share-limit denials and result filtering against
    //! a fake server.
    use super::*;
    use crate::state::{Limit, StateDir};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn fake_server(path: std::path::PathBuf) {
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let p = req["params"].clone();
                        let ws_of = |x: &str| if x.ends_with('1') { "w1" } else { "w2" };
                        let result = match req["method"].as_str().unwrap() {
                            "client.hello" => json!({"capabilities": ["*"]}),
                            "pane.get" => {
                                let id = p["pane"].as_str().unwrap_or("");
                                json!({"pane": {"id": id, "workspace": ws_of(id)}, "cwd": format!("/repo/{id}")})
                            }
                            "session.snapshot" => json!({
                                "at_seq": 1,
                                "workspaces": [{"id": "w1", "root_path": "/ws1"}, {"id": "w2", "root_path": "/ws2"}],
                                "tabs": [{"id": "t1", "workspace": "w1"}, {"id": "t2", "workspace": "w2"}],
                                "panes": [{"id": "p1", "tab": "t1", "workspace": "w1"}, {"id": "p2", "tab": "t2", "workspace": "w2"}],
                                "tasks": [{"id": "k1", "workspace": "w1"}, {"id": "k2", "workspace": "w2"}],
                                "runs": [], "interactions": []
                            }),
                            "task.get" => {
                                let id = p["task"].as_str().unwrap_or("");
                                json!({"task": {"id": id, "workspace": ws_of(id)}})
                            }
                            "task.check.get" => {
                                let id = p["check_run"].as_str().unwrap_or("");
                                json!({"check_run": {"id": id}, "task": if id == "c1" { "k1" } else { "k2" }})
                            }
                            "preview.get" => match p["preview"].as_str().unwrap_or("") {
                                "v1" => json!({"preview": {"id": "v1", "pane": "p1"}}),
                                "v2" => json!({"preview": {"id": "v2", "pane": "p2"}}),
                                _ => json!({"preview": {"id": "v3", "pane": null}}),
                            },
                            "preview.list" => json!({"previews": [
                                {"id": "v1", "pane": "p1"}, {"id": "v2", "pane": "p2"}, {"id": "v3", "pane": null}
                            ]}),
                            "screenshot.list" => {
                                json!({"echo": p, "count": 4, "total": 6, "screenshots": [
                                    {"id": "s1", "pane": "p1", "caption": "in"},
                                    {"id": "s2", "pane": "p2", "caption": "secret"},
                                    {"id": "s3", "pane": null, "caption": "no pane"},
                                    {"id": "s4", "pane": "p9", "caption": "closed pane"}
                                ]})
                            }
                            "screenshot.get" => match p["id"].as_str().unwrap_or("") {
                                "s1" => json!({"id": "s1", "pane": "p1", "data_b64": "aW4="}),
                                "s2" => json!({"id": "s2", "pane": "p2", "data_b64": "c2VjcmV0"}),
                                _ => json!({"id": "s3", "pane": null, "data_b64": "bm8="}),
                            },
                            "attention.list" => json!({
                                "items": [
                                    {"key": {"kind": "interaction", "id": "i1"}, "pane": "p1", "title": "in"},
                                    {"key": {"kind": "interaction", "id": "i2"}, "pane": "p2", "title": "secret title"},
                                    {"key": {"kind": "review", "id": "k1"}, "task": "k1", "pane": null},
                                    {"key": {"kind": "review", "id": "k2"}, "task": "k2", "pane": null}
                                ],
                                "coverage": {"complete": true, "notes": ["x"], "scope": "all", "excluded": 0},
                                "five_minute": {"keys": [{"kind": "interaction", "id": "i2"}, {"kind": "interaction", "id": "i1"}],
                                                "item_notes": [{"key": {"kind": "interaction", "id": "i2"}, "note": "n"}]}
                            }),
                            "agent.get" => {
                                let id = p["target"].as_str().unwrap_or("");
                                json!({"run": {"id": id, "pane": if id.ends_with('1') { "p1" } else { "p2" }}})
                            }
                            "search.query" => json!({"echo": p, "hits": [
                                {"pane": "p1", "text": "in"}, {"pane": "p2", "text": "secret"},
                                {"pane": "p9", "text": "archived"}, {"text": "no pane"}
                            ]}),
                            "worktree.create" | "workspace.create" => {
                                json!({"echo": p, "root_pane": {"id": "p9"}})
                            }
                            "interaction.get" => json!({"interaction": {
                                "id": "ib", "status": "Open", "kind": "approval", "decision_rev": 0,
                                "pane": "p1", "run": "", "answerable": true,
                                "native_ref": "boundary:copy_out:box1",
                                "action": {"tool": "boundary", "summary": "copy a.txt to the host outbox",
                                           "paths": ["a.txt"], "risk": "Medium"}
                            }}),
                            _ => json!({"echo": p}),
                        };
                        let out = json!({"jsonrpc": "2.0", "id": req["id"], "result": result})
                            .to_string()
                            + "\n";
                        if w.write_all(out.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
    }

    fn device(id: &str, scope: Scope, kind: &str, limit: Option<Limit>) -> Device {
        Device {
            id: id.into(),
            name: id.into(),
            platform: "test".into(),
            public: format!("k-{id}"),
            scope,
            paired_at: 0,
            vapid_private: None,
            push: vec![],
            prefs: Default::default(),
            push_failures: 0,
            kind: kind.into(),
            expires_at: None,
            limit,
            peer: None,
            supports_clear: false,
        }
    }

    async fn gateway(t: &tempfile::TempDir) -> Arc<crate::Gateway> {
        let sock = t.path().join("s.sock");
        fake_server(sock.clone());
        crate::Gateway::new(
            StateDir::open(t.path().join("gw")).unwrap(),
            crate::server::Server::new(sock),
        )
        .unwrap()
    }

    #[test]
    fn scope_table_for_workspace_methods() {
        for m in [
            "attention.list",
            "task.review.get",
            "task.review.candidates",
            "task.review.diff",
            "task.check.list",
            "task.check.get",
            "preview.list",
            "preview.get",
            "preview.url",
            "preview.status",
            "screenshot.list",
            "screenshot.get",
            "worktree.list",
            "git.log",
            "fs.list",
            "fs.read",
        ] {
            assert_eq!(required_scope(m), Some(Scope::View), "{m}");
            assert!(!is_mutating(m), "{m}");
            assert!(SERVER_READ_ONLY.contains(&m), "{m} must not need an actor");
        }
        assert_eq!(required_scope("attention.update"), Some(Scope::Approve));
        assert!(is_mutating("attention.update"));
        for m in [
            "task.check.run",
            "preview.open",
            "preview.promote",
            "preview.forget",
            "tab.rename",
            "tab.close",
            "tab.focus",
        ] {
            assert_eq!(required_scope(m), Some(Scope::Full), "{m}");
            assert!(is_mutating(m), "{m}");
        }
        // Shares never run checks, change attention or touch tabs.
        for m in [
            "task.check.run",
            "attention.update",
            "tab.rename",
            "tab.close",
            "tab.focus",
            "preview.status",
        ] {
            assert!(!kind_allows("share", m), "{m}");
            assert!(kind_allows("device", m), "{m}");
        }
        assert!(kind_allows("share", "tab.create"));
        assert!(kind_allows("share", "fs.read"));
        // Screenshots: shares read them (filtered to their limit); nobody adds or deletes them
        // through the gateway.
        for m in ["screenshot.list", "screenshot.get"] {
            assert!(kind_allows("share", m), "{m}");
        }
        for m in ["screenshot.add", "screenshot.delete", "screenshot.open"] {
            assert_eq!(required_scope(m), None, "{m}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn screenshots_stay_inside_the_share() {
        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t).await;
        let ids = |r: &Value| -> Vec<String> {
            r["screenshots"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_string())
                .collect()
        };

        // The owner's device sees everything, with the documented parameters only.
        let me = device("d1", Scope::View, "device", None);
        gw.add_device(me.clone()).unwrap();
        let call = Call {
            gw: &gw,
            device: &me,
        };
        let r = call
            .dispatch(
                "screenshot.list",
                json!({"environment": "agent", "limit": 5, "since_ms": 10, "junk": 1}),
            )
            .await
            .unwrap();
        assert_eq!(ids(&r), ["s1", "s2", "s3", "s4"]);
        assert_eq!(
            r["echo"],
            json!({"environment": "agent", "limit": 5, "since_ms": 10})
        );
        assert_eq!(r["total"], 6);
        let g = call
            .dispatch(
                "screenshot.get",
                json!({"id": "s2", "inline": true, "x": 1}),
            )
            .await
            .unwrap();
        assert_eq!(g["id"], "s2");
        assert_eq!(
            call.dispatch("screenshot.get", json!({}))
                .await
                .unwrap_err()
                .kind,
            "invalid_params"
        );

        for (name, limit) in [
            (
                "pane share",
                Limit {
                    workspace: None,
                    pane: Some("p1".into()),
                },
            ),
            (
                "workspace share",
                Limit {
                    workspace: Some("w1".into()),
                    pane: None,
                },
            ),
        ] {
            let share = device(&name.replace(' ', "-"), Scope::View, "share", Some(limit));
            gw.add_device(share.clone()).unwrap();
            let call = Call {
                gw: &gw,
                device: &share,
            };
            // Only the visible pane's records; no pane-less or closed-pane ones.
            let r = call.dispatch("screenshot.list", json!({})).await.unwrap();
            assert_eq!(ids(&r), ["s1"], "{name}");
            assert_eq!(r["count"], 1, "{name}");
            assert_eq!(r["total"], 3, "{name}: dropped records are not counted");
            assert!(!r.to_string().contains("secret"), "{name}");
            // The server is asked for the shared pane or workspace only.
            if name == "pane share" {
                assert_eq!(r["echo"]["pane"], "p1", "{name}");
            } else {
                assert_eq!(r["echo"]["workspace"], "w1", "{name}");
            }
            // Filters outside the limit are refused.
            for p in [
                json!({"pane": "p2"}),
                json!({"workspace": "w2"}),
                json!({"task": "k2"}),
            ] {
                assert_eq!(
                    call.dispatch("screenshot.list", p.clone())
                        .await
                        .unwrap_err()
                        .kind,
                    "forbidden",
                    "{name} {p}"
                );
            }
            // Another pane's (or no pane's) screenshot is not found, never returned.
            for id in ["s2", "s3"] {
                let e = call
                    .dispatch("screenshot.get", json!({"id": id, "inline": true}))
                    .await
                    .unwrap_err();
                assert_eq!(e.kind, "not_found", "{name} {id}");
                assert!(!e.message.contains("secret"), "{name} {id}");
            }
            let g = call
                .dispatch("screenshot.get", json!({"id": "s1", "inline": true}))
                .await
                .unwrap();
            assert_eq!(g["data_b64"], "aW4=", "{name}");
        }
    }

    #[test]
    fn path_picker_methods_are_full_scope_reads_never_shared() {
        for m in ["fs.browse", "repo.candidates"] {
            assert_eq!(required_scope(m), Some(Scope::Full), "{m}");
            assert!(!is_mutating(m), "{m}");
            assert!(SERVER_READ_ONLY.contains(&m), "{m}");
            assert!(!kind_allows("share", m), "{m}");
            assert!(!kind_allows("handoff", m), "{m}");
            assert!(kind_allows("device", m), "{m}");
        }
    }

    #[test]
    fn incoming_handoffs_are_for_full_devices() {
        for m in [
            "handoff.incoming.list",
            "handoff.incoming.get",
            "handoff.accept",
            "handoff.decline",
            "handoff.resume",
            "handoff.prefs",
        ] {
            assert_eq!(required_scope(m), Some(Scope::Full), "{m}");
            assert!(kind_allows("device", m), "{m}");
            assert!(!kind_allows("share", m), "{m}");
            assert!(
                !kind_allows("handoff", m),
                "{m}: an invitation only delivers"
            );
        }
        assert!(!is_mutating("handoff.incoming.list"));
        assert!(!is_mutating("handoff.incoming.get"));
        assert!(is_mutating("handoff.accept"));
        assert!(is_mutating("handoff.decline"));
        assert!(is_mutating("handoff.resume"));
    }

    #[test]
    fn task_and_tab_limits() {
        let ws = Allowed {
            workspace: Some("w1".into()),
            pane: None,
        };
        let pane = Allowed {
            workspace: None,
            pane: Some("p1".into()),
        };
        assert!(ws.task_ok(Some("w1")));
        assert!(!ws.task_ok(Some("w2")));
        assert!(!ws.task_ok(None));
        assert!(!pane.task_ok(Some("w1")), "pane-only shares see no tasks");
        assert!(ws.tab_ok(Some("w1"), &[]));
        assert!(!ws.tab_ok(Some("w2"), &["p1".into()]));
        assert!(pane.tab_ok(Some("w2"), &["p1".into()]));
        assert!(!pane.tab_ok(Some("w1"), &["p2".into()]));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn control_shares_stay_inside_their_pane() {
        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t).await;
        let share = device(
            "s2",
            Scope::Full,
            "share",
            Some(Limit {
                workspace: None,
                pane: Some("p1".into()),
            }),
        );
        gw.add_device(share.clone()).unwrap();
        let call = Call {
            gw: &gw,
            device: &share,
        };
        for (m, p) in [
            ("share.create", json!({"kind": "share", "pane": "p1"})),
            ("share.list", json!({})),
            ("share.revoke", json!({"id": "x"})),
            ("devices.revoke", json!({"device": "d1"})),
            ("peer.invite", json!({})),
            ("handoff.send", json!({"pane": "p1", "peer": "x"})),
            ("handoff.incoming.list", json!({})),
            ("auth.list", json!({})),
            (
                "auth.approve.decide",
                json!({"request": "r", "decision": "approve"}),
            ),
            ("pane.send_text", json!({"pane": "p2", "text": "ls"})),
            ("tab.create", json!({"pane": "p1", "workspace": "w2"})),
            ("preview.open", json!({"url": "https://example.org"})),
            ("prefs.set", json!({"host": {"dnd_until": 4102444800u64}})),
            (
                "agent.set_model",
                json!({"target": "r1", "model": "m", "scope": "default"}),
            ),
        ] {
            assert_eq!(
                call.dispatch(m, p).await.unwrap_err().kind,
                "forbidden",
                "{m}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn share_limit_denials_and_filtering() {
        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t).await;
        let share = device(
            "s1",
            Scope::Approve,
            "share",
            Some(Limit {
                workspace: Some("w1".into()),
                pane: None,
            }),
        );
        gw.add_device(share.clone()).unwrap();
        let call = Call {
            gw: &gw,
            device: &share,
        };
        let forbidden = |r: ApiResult, what: &str| {
            assert_eq!(r.unwrap_err().kind, "forbidden", "{what}");
        };
        for (m, p) in [
            ("task.review.get", json!({"task": "k2"})),
            ("task.review.candidates", json!({"task": "k2"})),
            ("task.review.diff", json!({"task": "k2", "path": "a.rs"})),
            ("task.check.list", json!({"task": "k2"})),
            ("task.check.get", json!({"check_run": "c2"})),
            ("preview.get", json!({"preview": "v2"})),
            ("preview.url", json!({"preview": "v2"})),
            ("preview.url", json!({"preview": "v3"})),
            ("preview.list", json!({"task": "k2"})),
            ("preview.list", json!({"pane": "p2"})),
            ("git.log", json!({"pane": "p2"})),
            (
                "git.diff",
                json!({"pane": "p2", "file": "a", "range": "a..b"}),
            ),
            ("fs.list", json!({"pane": "p2"})),
            ("fs.read", json!({"pane": "p2", "path": "a.txt"})),
            ("worktree.list", json!({"pane": "p2"})),
            ("worktree.list", json!({"workspace": "w2"})),
            ("worktree.list", json!({"cwd": "/"})),
            ("worktree.list", json!({"pane": "p1", "repo": "/etc"})),
            // An allowed pane can't smuggle an outside task.
            ("fs.read", json!({"pane": "p1", "path": "a", "task": "k2"})),
        ] {
            forbidden(call.dispatch(m, p.clone()).await, &format!("{m} {p}"));
        }
        // Inside the limit.
        for (m, p) in [
            ("task.review.get", json!({"task": "k1"})),
            ("task.check.get", json!({"check_run": "c1"})),
            ("preview.get", json!({"preview": "v1"})),
            ("fs.list", json!({"pane": "p1", "path": "src"})),
            ("git.log", json!({"pane": "p1", "base": "main"})),
        ] {
            call.dispatch(m, p.clone())
                .await
                .unwrap_or_else(|e| panic!("{m} {p}: {e:?}"));
        }
        // The repository-wide worktree list would show sibling checkouts: never for shares.
        forbidden(
            call.dispatch("worktree.list", json!({"workspace": "w1"}))
                .await,
            "worktree.list own workspace",
        );
        // A machine selector could re-target an allowed handle elsewhere after the check.
        forbidden(
            call.dispatch("preview.get", json!({"preview": "v1", "machine": "other"}))
                .await,
            "preview on another machine",
        );
        forbidden(
            call.dispatch("preview.url", json!({"preview": "v1", "machine": "other"}))
                .await,
            "preview url on another machine",
        );
        // Lists are filtered to the limit.
        let a = call.dispatch("attention.list", json!({})).await.unwrap();
        let ids: Vec<&str> = a["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["key"]["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["i1", "k1"]);
        assert_eq!(a["coverage"]["excluded"], 2);
        assert_eq!(a["coverage"]["notes"], json!([]));
        assert_eq!(
            a["five_minute"]["keys"],
            json!([{"kind": "interaction", "id": "i1"}])
        );
        assert_eq!(a["five_minute"]["item_notes"], json!([]));
        assert!(!a.to_string().contains("secret title"));
        let pv = call.dispatch("preview.list", json!({})).await.unwrap();
        assert_eq!(pv["previews"], json!([{"id": "v1", "pane": "p1"}]));

        // A pane-only share sees no tasks.
        let pane_share = device(
            "s2",
            Scope::View,
            "share",
            Some(Limit {
                workspace: None,
                pane: Some("p1".into()),
            }),
        );
        gw.add_device(pane_share.clone()).unwrap();
        let call = Call {
            gw: &gw,
            device: &pane_share,
        };
        forbidden(
            call.dispatch("task.review.get", json!({"task": "k1"}))
                .await,
            "pane share task",
        );
        forbidden(
            call.dispatch("worktree.list", json!({"workspace": "w1"}))
                .await,
            "pane share workspace",
        );
        let a = call.dispatch("attention.list", json!({})).await.unwrap();
        assert_eq!(a["items"].as_array().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn passthrough_params_are_whitelisted() {
        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t).await;
        let me = device("d1", Scope::Full, "device", None);
        gw.add_device(me.clone()).unwrap();
        let call = Call {
            gw: &gw,
            device: &me,
        };
        let echo = |r: ApiResult| r.unwrap()["echo"].clone();
        assert_eq!(
            echo(
                call.dispatch("fs.read", json!({"pane": "p1", "path": "a", "junk": 1}))
                    .await
            ),
            json!({"pane": "p1", "path": "a"})
        );
        assert_eq!(
            echo(
                call.dispatch(
                    "git.diff",
                    json!({"pane": "p1", "file": "a", "base": "main", "range": "a..b", "x": 1})
                )
                .await
            ),
            json!({"pane": "p1", "file": "a", "base": "main", "range": "a..b"})
        );
        let up = echo(
            call.dispatch(
                "attention.update",
                json!({"key": {"kind": "review", "id": "k1"}, "snooze_until_ms": null, "op_id": "o"}),
            )
            .await,
        );
        assert!(up["snooze_until_ms"].is_null() && up.get("snooze_until_ms").is_some());
        assert_eq!(up["actor"], "gateway:d1");
        let run = echo(
            call.dispatch(
                "task.check.run",
                json!({"task": "k1", "subject": "s", "check": "c", "authorize": true, "op_id": "o2"}),
            )
            .await,
        );
        assert_eq!(run["idempotency_key"], "gw:d1:o2");
        assert_eq!(run["authorize"], true);
        assert_eq!(
            echo(
                call.dispatch(
                    "tab.rename",
                    json!({"tab": "t1", "title": "x", "pane": "p9"})
                )
                .await
            )["pane"],
            Value::Null
        );
        assert_eq!(
            echo(call.dispatch("worktree.list", json!({"cwd": "/r"})).await),
            json!({"cwd": "/r"})
        );
        assert_eq!(
            echo(
                call.dispatch(
                    "preview.open",
                    json!({"preview": "v1", "split": "down", "headless": true})
                )
                .await
            ),
            json!({"preview": "v1", "split": "down", "actor": "gateway:d1"})
        );
    }

    /// Methods exposed for the app's catch-up, search, sandbox and preview screens.
    const VIEW_READS: &[&str] = &[
        "agent.turns",
        "assistant.status",
        "assistant.get",
        "desk.search",
        "search.query",
        "sandbox.list",
        "sandbox.status",
        "browser.list",
        "browser.status",
        "browser.screencast_frame",
    ];
    const FULL_ACTIONS: &[&str] = &[
        "worktree.create",
        "workspace.create",
        "assistant.generate",
        "assistant.confirm",
        "assistant.cancel",
        "browser.take_over",
        "browser.release",
        "browser.click",
        "browser.type",
        "browser.press",
        "browser.navigate",
    ];

    #[test]
    fn scope_table_for_app_lane_methods() {
        for m in VIEW_READS {
            assert_eq!(required_scope(m), Some(Scope::View), "{m}");
            assert!(!is_mutating(m), "{m}");
            assert!(SERVER_READ_ONLY.contains(m), "{m} must not need an actor");
        }
        // Viewer bookkeeping: no op_id, but not a plain server read either (screencast.rs).
        for m in ["browser.attach_screencast", "browser.detach_screencast"] {
            assert_eq!(required_scope(m), Some(Scope::View), "{m}");
            assert!(!is_mutating(m), "{m}");
            assert!(!SERVER_READ_ONLY.contains(&m), "{m}");
        }
        for m in FULL_ACTIONS {
            assert_eq!(required_scope(m), Some(Scope::Full), "{m}");
            assert!(is_mutating(m), "{m} needs an op_id");
            assert!(
                !SERVER_READ_ONLY.contains(m),
                "{m} is re-authorized with an actor"
            );
        }
        // Never exposed: consent, provider settings, goals, raw scripts, box actions.
        for m in [
            "assistant.consent",
            "assistant.revoke",
            "assistant.test",
            "assistant.purge",
            "goal.list",
            "goal.get",
            "goal.approve",
            "goal.cancel",
            "goal.create",
            "goal.plan",
            "goal.plan_submit",
            "goal.start",
            "goal.step_done",
            "browser.eval",
            "browser.open",
            "browser.screencast",
            "sandbox.request",
            "sandbox.push",
            "sandbox.copy_out",
            "desk.open",
            "desk.resume",
        ] {
            assert_eq!(required_scope(m), None, "{m}");
        }
        // Host-wide: never for share devices (search.query is filtered instead).
        for m in VIEW_READS.iter().chain(FULL_ACTIONS) {
            if *m == "search.query" || *m == "agent.turns" {
                assert!(kind_allows("share", m), "{m}");
                continue;
            }
            assert!(host_wide(m), "{m}");
            assert!(!kind_allows("share", m), "{m}");
            assert!(kind_allows("device", m), "{m}");
            assert!(!kind_allows("peer", m), "{m}");
        }
        assert!(!kind_allows("share", "browser.attach_screencast"));
    }

    fn limited(id: &str, scope: Scope, workspace: Option<&str>, pane: Option<&str>) -> Device {
        device(
            id,
            scope,
            "device",
            Some(Limit {
                workspace: workspace.map(str::to_string),
                pane: pane.map(str::to_string),
            }),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn limited_devices_get_no_host_wide_methods() {
        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t).await;
        // A limited own device (not only shares) is checked the same way.
        let d = limited("l1", Scope::Full, Some("w1"), None);
        gw.add_device(d.clone()).unwrap();
        let call = Call {
            gw: &gw,
            device: &d,
        };
        for (m, p) in [
            ("worktree.create", json!({"workspace": "w1", "branch": "b"})),
            ("workspace.create", json!({"cwd": "/tmp"})),
            ("desk.search", json!({"text": "x"})),
            ("assistant.status", json!({})),
            (
                "assistant.generate",
                json!({"operation": "briefing", "workspace": "w1"}),
            ),
            ("sandbox.list", json!({})),
            ("browser.list", json!({})),
            ("browser.attach_screencast", json!({"session": "b1"})),
            ("browser.take_over", json!({"session": "b1"})),
            (
                "agent.start",
                json!({"workspace": "w1", "harness": "claude", "worktree": {"branch": "b"}}),
            ),
            (
                "agent.start",
                json!({"workspace": "w1", "harness": "claude", "new_workspace": {"cwd": "/tmp"}}),
            ),
            // A run outside the limit.
            ("agent.turns", json!({"run": "r2"})),
            ("search.query", json!({"q": "x", "pane": "p2"})),
            ("search.query", json!({"q": "x", "workspace": "w2"})),
        ] {
            assert_eq!(
                call.dispatch(m, p.clone()).await.unwrap_err().kind,
                "forbidden",
                "{m} {p}"
            );
        }
        // Inside the limit.
        let r = call
            .dispatch("agent.turns", json!({"run": "r1", "limit": 5, "x": 1}))
            .await
            .unwrap();
        assert_eq!(r["echo"], json!({"run": "r1", "limit": 5}));
        // Search is narrowed to the shared workspace and its hits to visible panes.
        let r = call
            .dispatch("search.query", json!({"q": "x", "machine": null}))
            .await
            .unwrap();
        assert_eq!(r["echo"]["workspace"], "w1");
        assert_eq!(r["hits"], json!([{"pane": "p1", "text": "in"}]));
        assert!(!r.to_string().contains("secret"));
        // A pane-only device searches its pane.
        let d = limited("l2", Scope::View, None, Some("p1"));
        gw.add_device(d.clone()).unwrap();
        let call = Call {
            gw: &gw,
            device: &d,
        };
        let r = call
            .dispatch("search.query", json!({"q": "x"}))
            .await
            .unwrap();
        assert_eq!(r["echo"]["pane"], "p1");
        assert!(r["echo"].get("workspace").is_none());
        assert_eq!(r["hits"].as_array().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn new_agents_in_a_worktree_or_folder() {
        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t).await;
        let me = device("d1", Scope::Full, "device", None);
        gw.add_device(me.clone()).unwrap();
        let call = Call {
            gw: &gw,
            device: &me,
        };
        let r = call
            .dispatch(
                "agent.start",
                json!({"pane": "p1", "harness": "claude", "prompt": "hi",
                       "worktree": {"branch": "feat/x", "base": "main", "path": "/etc"}}),
            )
            .await
            .unwrap();
        // The worktree's first pane runs the agent.
        assert_eq!(r["pane"], "p9");
        assert_eq!(
            r["echo"],
            json!({"pane": "p9", "harness": "claude", "prompt": "hi", "actor": "gateway:d1"})
        );
        let r = call
            .dispatch(
                "agent.start",
                json!({"harness": "codex", "new_workspace": {"cwd": "/tmp/x", "name": "x", "command": ["sh"]}}),
            )
            .await
            .unwrap();
        assert_eq!(r["pane"], "p9");
        // worktree.create runs in the source pane's directory; no path or root override.
        let r = call
            .dispatch(
                "worktree.create",
                json!({"pane": "p1", "branch": "b", "path": "/etc", "root": "/"}),
            )
            .await
            .unwrap();
        assert_eq!(
            r["echo"],
            json!({"branch": "b", "cwd": "/repo/p1", "actor": "gateway:d1"})
        );
        let r = call
            .dispatch(
                "workspace.create",
                json!({"cwd": "/tmp/x", "command": ["sh"], "layout": {}}),
            )
            .await
            .unwrap();
        assert_eq!(r["echo"], json!({"cwd": "/tmp/x", "actor": "gateway:d1"}));
        assert_eq!(
            call.dispatch(
                "agent.start",
                json!({"harness": "claude", "worktree": {"base": "main"}, "pane": "p1"})
            )
            .await
            .unwrap_err()
            .kind,
            "invalid_params"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn assistant_and_boundary_requests() {
        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t).await;
        let me = device("d1", Scope::Full, "device", None);
        gw.add_device(me.clone()).unwrap();
        let call = Call {
            gw: &gw,
            device: &me,
        };
        // Only the catch-up and reply operations, with the host's own profile and sources.
        for op in ["review_summary", "handoff", "navigate", "pane_title"] {
            assert_eq!(
                call.dispatch("assistant.generate", json!({"operation": op}))
                    .await
                    .unwrap_err()
                    .kind,
                "forbidden",
                "{op}"
            );
        }
        let r = call
            .dispatch(
                "assistant.generate",
                json!({"operation": "reply_suggestions", "pane": "p1", "include_screen": true,
                       "profile": "expensive", "remote_sources": [{}], "inputs": {"pane": "p2"}, "op_id": "o1"}),
            )
            .await
            .unwrap();
        assert_eq!(
            r["echo"],
            json!({"operation": "reply_suggestions", "pane": "p1", "include_screen": true,
                   "idempotency_key": "gw:d1:o1", "actor": "gateway:d1"})
        );
        // Boundary requests show what is asked and take allow (once) or deny only.
        let r = call
            .dispatch("interaction.get", json!({"interaction": "ib"}))
            .await
            .unwrap();
        assert_eq!(
            r["interaction"]["boundary"],
            json!({"kind": "copy_out", "pane": "p1", "path": "a.txt"})
        );
        assert!(!batch_eligible(&r["interaction"]));
        for bad in [
            json!({"interaction": "ib", "decision_rev": 0, "decision": "allow_always"}),
            json!({"interaction": "ib", "decision_rev": 0, "text": "sure"}),
            json!({"interaction": "ib", "decision_rev": 0, "decision": "allow", "choices": {}}),
        ] {
            assert_eq!(
                call.dispatch("interaction.answer", bad.clone())
                    .await
                    .unwrap_err()
                    .kind,
                "invalid_params",
                "{bad}"
            );
        }
        let r = call
            .dispatch(
                "interaction.answer",
                json!({"interaction": "ib", "decision_rev": 0, "decision": "allow", "op_id": "o2"}),
            )
            .await
            .unwrap();
        assert_eq!(r["echo"]["decision"], "allow");
        assert_eq!(r["echo"]["expected_decision_rev"], 0);
    }

    #[test]
    fn boundary_push_requests_name_remote_and_branch() {
        let it = json!({"pane": "p1", "native_ref": "boundary:push:box1",
                        "action": {"tool": "boundary", "summary": "git push origin feat/x (from the host, hooks off)", "paths": []}});
        assert_eq!(
            boundary_of(&it),
            Some(json!({"kind": "push", "pane": "p1", "remote": "origin", "branch": "feat/x"}))
        );
        let mut plain = it.clone();
        plain["action"]["tool"] = "Bash".into();
        assert_eq!(boundary_of(&plain), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn browser_input_needs_a_take_over_and_previews_need_an_attach() {
        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t).await;
        let me = device("d1", Scope::Full, "device", None);
        let other = device("d2", Scope::Full, "device", None);
        gw.add_device(me.clone()).unwrap();
        let mut other = other;
        other.public = "k-other".into();
        gw.add_device(other.clone()).unwrap();
        let call = Call {
            gw: &gw,
            device: &me,
        };
        let theirs = Call {
            gw: &gw,
            device: &other,
        };
        let conflict = |r: ApiResult| r.unwrap_err().kind;
        assert_eq!(
            conflict(
                call.dispatch("browser.screencast_frame", json!({"session": "b1"}))
                    .await
            ),
            "conflict"
        );
        call.dispatch("browser.attach_screencast", json!({"session": "b1"}))
            .await
            .unwrap();
        let f = call
            .dispatch(
                "browser.screencast_frame",
                json!({"session": "b1", "after_seq": 3, "target": "r2"}),
            )
            .await
            .unwrap();
        assert_eq!(f["echo"], json!({"session": "b1", "after_seq": 3}));
        assert_eq!(gw.screencasts.live(), vec!["b1".to_string()]);
        assert_eq!(
            conflict(
                call.dispatch("browser.click", json!({"session": "b1", "x": 1, "y": 2}))
                    .await
            ),
            "conflict"
        );
        // A device that is not watching cannot take over: its take-over would outlive any lease.
        assert_eq!(
            conflict(
                theirs
                    .dispatch("browser.take_over", json!({"session": "b1", "op_id": "t"}))
                    .await
            ),
            "conflict"
        );
        call.dispatch("browser.take_over", json!({"session": "b1", "op_id": "o"}))
            .await
            .unwrap();
        // Only the device that took over types into the page.
        assert_eq!(
            conflict(
                theirs
                    .dispatch("browser.type", json!({"session": "b1", "text": "x"}))
                    .await
            ),
            "conflict"
        );
        let r = call
            .dispatch(
                "browser.type",
                json!({"session": "b1", "text": "hello", "submit": true, "js": "alert(1)"}),
            )
            .await
            .unwrap();
        assert_eq!(
            r["echo"],
            json!({"session": "b1", "text": "hello", "submit": true, "actor": "gateway:d1"})
        );
        // The device goes away: it stops watching and its take-over ends.
        crate::screencast::device_gone(&gw, "d1").await;
        assert!(gw.screencasts.live().is_empty());
        assert_eq!(gw.screencasts.taken_by("b1"), None);
    }
}
