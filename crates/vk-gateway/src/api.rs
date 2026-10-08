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
    "worktree.list",
    "fs.browse",
    "repo.candidates",
    "handoff.incoming.list",
    "handoff.incoming.get",
    "handoff.jobs",
    "handoff.peers",
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
        | "worktree.list"
        | "git.log"
        | "fs.list"
        | "fs.read" => View,
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
        _ => return None,
    })
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
    s(it, "kind") == Some("approval")
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
                || method.starts_with("tab.") && method != "tab.create"
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
            let _ = self.gw.reload_devices();
            if self.gw.device(&self.device.id).is_none_or(|d| d.expired()) {
                return Err(ApiError::new(
                    "forbidden",
                    "this device is no longer authorized",
                ));
            }
        }
        if SERVER_READ_ONLY.contains(&method) {
            return self.gw.server.call(method, params).await;
        }
        self.gw.server.call_as(&self.actor(), method, params).await
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

    /// Add `harness` and `repo_root` for grouping.
    async fn enrich(&self, it: &mut Value) {
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
        match method {
            "tab.create" | "agent.start" if s(p, "pane").is_none() => {
                if allowed.pane.is_some() || s(p, "workspace") != allowed.workspace.as_deref() {
                    return Err(deny());
                }
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

    /// Every selector present (`pane`, `target`, `interaction`, `task`, `check_run`, `tab`,
    /// `preview`) must resolve inside the limit, so a request can't pair an allowed pane with an
    /// outside run, interaction, task, tab or preview.
    async fn check_selectors(&self, allowed: &Allowed, p: &Value) -> Result<(), ApiError> {
        let deny = || ApiError::new("forbidden", "outside what was shared with you");
        for key in [
            "pane",
            "target",
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
                "attention.list" | "preview.list" => {
                    let (panes, tasks) = self.visible(a).await?;
                    Self::filter_list(method, &mut r, &panes, &tasks);
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
            "agent.harnesses" => self.server("agent.harnesses", json!({})).await,
            "agent.transcript" => {
                // Server pages natively for gateway clients: {turns:[{n, ts, items}], next_before}.
                req(&p, "target")?;
                self.server("agent.transcript", pick(&p, &["target", "before", "limit"]))
                    .await
            }
            "agent.start" => {
                let pane = match s(&p, "pane") {
                    Some(pane) => pane.to_string(),
                    None => {
                        let t = self
                            .server("tab.create", pick(&p, &["workspace", "cwd"]))
                            .await?;
                        t.get("root_pane")
                            .and_then(|r| r.get("id").or(Some(r)))
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| {
                                ApiError::new("internal", "tab.create returned no root pane")
                            })?
                            .to_string()
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
                self.server(method, pick(&p, &["pane", "path"])).await
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
                crate::push::vapid_public(
                    &vk_e2e::b64::decode(&vapid).map_err(|_| ApiError::invalid("vapid_private"))?,
                )
                .map_err(|_| ApiError::invalid("vapid_private"))?;
                self.gw
                    .update_device(&self.device.id, |d| {
                        d.vapid_private = Some(vapid.clone());
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
            "devices.list" => {
                let me = &self.device.id;
                let list: Vec<Value> = self
                    .gw
                    .devices()
                    .iter()
                    .map(|d| json!({"id": d.id, "name": d.name, "platform": d.platform, "scope": d.scope, "paired_at": d.paired_at, "fingerprint": d.fingerprint(), "push": !d.push.is_empty(), "this": &d.id == me, "kind": d.kind, "expires_at": d.expires_at, "limit": d.limit, "peer": d.peer}))
                    .collect();
                Ok(json!({"devices": list}))
            }
            "devices.revoke" => {
                let id = req(&p, "device")?;
                if id == self.device.id {
                    return Err(ApiError::invalid(
                        "a device cannot revoke itself here; use Forget host",
                    ));
                }
                self.gw.revoke(id).await.map_err(internal)?;
                Ok(json!({}))
            }
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
                    _ => return Err(ApiError::invalid("a share is view or approve")),
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
        assert_eq!(required_scope("pane.send_keys"), Some(Scope::Full));
        assert_eq!(required_scope("server.stop"), None);
        assert!(is_mutating("interaction.answer"));
        assert!(!is_mutating("pane.read"));
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
}
