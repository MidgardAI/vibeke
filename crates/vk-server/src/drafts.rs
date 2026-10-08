//! Drafts composer (research R3, spec 07 `draft.*` / `notes.*`): several persistent drafts per
//! workspace or task, with file and screenshot references, kept outside the live agent input,
//! plus a per-workspace notes document that is never sent unless the user includes it.
//!
//! `draft.send` reuses task messages' guarded prompt-input path (15 §9) for any live run, with
//! no tracked task required: the exact run and native conversation are fixed when the send
//! starts, safety is rechecked under the pane's input lock, zero bytes are written when it is
//! unsafe, delivery is confirmed only by a matching turn, and anything ambiguous stays
//! `delivery_unknown`. The draft is kept after an uncertain delivery; a retry needs
//! `draft.reconcile` first. Receipts are owner-aware (`review::receipts`).
//!
//! Draft and notes text are content: they live in the state tables only; events carry ids,
//! revisions and sizes.

use crate::Server;
use crate::api::{Ctx, R, err, internal, invalid, not_found, req, s};
use crate::core::Tx;
use crate::review::receipts;
use crate::tracking::{MessageState, conflict};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use vk_proto::model::*;
use vk_proto::rpc::{ErrorKind, RpcError};

pub const K_DRAFT: &str = "draft";
pub const K_NOTES: &str = "workspace_notes";

pub const METHODS: &[(&str, bool)] = &[
    ("draft.create", true),
    ("draft.update", true),
    ("draft.get", false),
    ("draft.list", false),
    ("draft.reorder", true),
    ("draft.delete", true),
    ("draft.combine", true),
    ("draft.check", false),
    ("draft.send", true),
    ("draft.reconcile", true),
    ("notes.get", false),
    ("notes.set", true),
];

pub const MAX_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_ATTACHMENTS: usize = 20;
pub const MAX_DRAFTS_PER_SCOPE: usize = 200;
/// Delivered (archived) drafts are removed after this long.
pub const ARCHIVE_RETENTION_MS: i64 = 30 * 86_400_000;
const MAX_SENDS_KEPT: usize = 10;

#[derive(Default)]
pub struct State {
    /// Drafts whose delivery attempt is running in this server process.
    inflight: Mutex<HashSet<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    /// `file` | `screenshot`.
    pub kind: String,
    /// Absolute path on this machine (screenshots uploaded as data live in the pane inbox).
    pub path: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftSend {
    pub id: String,
    pub run: String,
    pub pane: String,
    pub harness: String,
    pub native_conversation_id: String,
    /// Exact text written to the agent (draft + attachment references + notes if included).
    pub text: String,
    pub include_notes: bool,
    pub state: MessageState,
    pub detail: Option<String>,
    pub idempotency_key: String,
    /// Receipt owner (`user` / `pane:<id>`).
    pub owner: String,
    /// The first turn number that can count as delivery.
    pub turn_baseline: u32,
    /// `draft.reconcile` inspected this (uncertain) attempt; only then may a retry be sent.
    #[serde(default)]
    pub reconciled: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub id: String,
    /// `workspace` | `task`.
    pub scope: String,
    pub scope_id: String,
    /// Workspace the draft belongs to (authorization), if known.
    pub workspace: Option<String>,
    pub title: Option<String>,
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub order: f64,
    pub rev: u32,
    #[serde(default)]
    pub combined_from: Vec<String>,
    #[serde(default)]
    pub sends: Vec<DraftSend>,
    /// Delivered and archived (no longer listed by default).
    #[serde(default)]
    pub archived: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl Draft {
    fn last_send(&self) -> Option<&DraftSend> {
        self.sends.last()
    }
    fn sending(&self) -> bool {
        self.last_send()
            .is_some_and(|x| x.state == MessageState::Sending)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notes {
    pub workspace: String,
    pub text: String,
    pub rev: u32,
    pub updated_at_ms: i64,
}

fn now() -> i64 {
    vk_store::now_ms()
}

pub(crate) fn user_ctx() -> Ctx {
    Ctx {
        client_id: "local".into(),
        kind: "internal".into(),
        pane_scope: None,
        remote: false,
    }
}

fn denied(method: &str) -> RpcError {
    err(
        ErrorKind::PermissionDenied,
        format!("{method}: outside this pane's workspace"),
    )
    .details(json!({"scope": "pane"}))
}

/// The caller pane's workspace for pane-scoped callers (`None` for full-scope callers).
pub(crate) fn caller_ws(
    server: &Server,
    ctx: &Ctx,
    method: &str,
) -> Result<Option<String>, RpcError> {
    let Some(pane) = &ctx.pane_scope else {
        return Ok(None);
    };
    server
        .with_core(|c| c.pane(pane).map(|p| p.workspace.clone()))
        .map(Some)
        .ok_or_else(|| denied(method))
}

fn bounded_text(t: &str, what: &str) -> Result<String, RpcError> {
    if t.len() > MAX_TEXT_BYTES {
        return Err(invalid(format!(
            "{what} is larger than {} KiB",
            MAX_TEXT_BYTES / 1024
        )));
    }
    Ok(t.to_string())
}

fn put(tx: &mut Tx, d: &Draft) {
    if d.archived {
        tx.m.close(K_DRAFT, &d.id, None, d);
    } else {
        tx.m.put(K_DRAFT, &d.id, None, d);
    }
}

fn meta(d: &Draft) -> Value {
    json!({"rev": d.rev, "bytes": d.text.len(), "attachments": d.attachments.len()})
}

fn subject(d: &Draft) -> Value {
    json!({"draft": d.id, "scope": d.scope, "scope_id": d.scope_id, "workspace": d.workspace})
}

/// Load a draft the caller may see; a send interrupted by a restart becomes
/// `delivery_unknown` (never assumed either way).
fn load(server: &Server, scope: Option<&str>, id: &str, method: &str) -> Result<Draft, RpcError> {
    let mut d = server
        .with_core(|c| c.store.get::<Draft>(K_DRAFT, id).ok().flatten())
        .ok_or_else(|| not_found("draft", id))?;
    if let Some(ws) = scope
        && d.workspace.as_deref() != Some(ws)
    {
        return Err(denied(method));
    }
    if d.sending() && !server.drafts.inflight.lock().unwrap().contains(&d.id) {
        let x = d.sends.last_mut().unwrap();
        x.state = MessageState::DeliveryUnknown;
        x.detail = Some("the server restarted during delivery".into());
        x.updated_at_ms = now();
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        put(&mut tx, &d);
        tx.event(
            "draft.delivery_unknown",
            subject(&d),
            json!({"send": d.sends.last().map(|x| x.id.clone())}),
        );
        server.commit(&mut c, tx).map_err(internal)?;
    }
    Ok(d)
}

/// Resolve `{scope, id}` to (scope, scope_id, workspace).
fn resolve_scope(
    server: &Server,
    ctx: &Ctx,
    p: &Value,
) -> Result<(String, String, Option<String>), RpcError> {
    match s(p, "scope").unwrap_or("workspace") {
        "workspace" => {
            let ws = crate::api::resolve_ws(server, ctx, s(p, "id").or(s(p, "workspace")))?;
            Ok(("workspace".into(), ws.id.clone(), Some(ws.id)))
        }
        "task" => {
            let t = s(p, "id")
                .or(s(p, "task"))
                .ok_or_else(|| invalid("task scope needs `id`"))?;
            let task = crate::tracking::find_task(server, t)?;
            Ok(("task".into(), task.id.clone(), task.workspace.clone()))
        }
        other => Err(invalid(format!(
            "scope `{other}`: expected workspace or task"
        ))),
    }
}

fn attachments_from(server: &Server, ctx: &Ctx, p: &Value) -> Result<Vec<Attachment>, RpcError> {
    let Some(list) = p.get("attachments").and_then(Value::as_array) else {
        return Ok(vec![]);
    };
    if list.len() > MAX_ATTACHMENTS {
        return Err(invalid(format!("at most {MAX_ATTACHMENTS} attachments")));
    }
    list.iter()
        .map(|a| attachment_from(server, ctx, a))
        .collect()
}

/// `{kind: file|screenshot, path}` (a path on this machine), `{kind, blob: <hash>}` (a file
/// stored by `blob.put`) or `{kind, data_b64, name?}` (stored now in the pane inbox). A
/// pane-scoped caller cannot name a `path`: the existence check would be a file oracle.
fn attachment_from(server: &Server, ctx: &Ctx, a: &Value) -> Result<Attachment, RpcError> {
    let kind = s(a, "kind").unwrap_or("file");
    if !matches!(kind, "file" | "screenshot") {
        return Err(invalid(format!(
            "attachment kind `{kind}`: expected file or screenshot"
        )));
    }
    let path = if let Some(path) = s(a, "path") {
        if ctx.pane_scope.is_some() {
            return Err(err(
                ErrorKind::PermissionDenied,
                "attachment path needs a user client; attach the content with data_b64",
            ));
        }
        let pth = Path::new(path);
        if !pth.is_absolute() || !pth.exists() {
            return Err(invalid(format!(
                "attachment {path}: not an existing absolute path on this machine (remote clients upload it with data_b64)"
            )));
        }
        path.to_string()
    } else if let Some(hash) = s(a, "blob") {
        let h: String = hash.chars().take(12).collect();
        if h.len() < 12 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(invalid(
                "attachment blob: expected the hash returned by blob.put",
            ));
        }
        let dir = crate::paths::Paths::inbox().join(&h);
        std::fs::read_dir(&dir)
            .ok()
            .and_then(|rd| rd.flatten().map(|e| e.path()).find(|p| p.is_file()))
            .map(|p| p.to_string_lossy().into_owned())
            .ok_or_else(|| not_found("blob", hash))?
    } else if a.get("data_b64").is_some() {
        let v = crate::api::blob_put(server, ctx, a)?;
        v["path"].as_str().unwrap_or("").to_string()
    } else {
        return Err(invalid("attachment needs path, blob or data_b64"));
    };
    Ok(Attachment {
        kind: kind.into(),
        path,
        label: s(a, "label").map(str::to_string),
    })
}

fn scope_drafts(server: &Server, scope: &str, scope_id: &str) -> Vec<Draft> {
    let mut v: Vec<Draft> = server.with_core(|c| {
        c.store
            .load_by_field::<Draft>(K_DRAFT, "$.scope_id", scope_id)
            .unwrap_or_default()
    });
    v.retain(|d| d.scope == scope && !d.archived);
    v.sort_by(|a, b| a.order.total_cmp(&b.order));
    v
}

/// Create a draft (also used by `desk.resume` for a context package).
pub(crate) fn create_internal(
    server: &Server,
    ctx: &Ctx,
    scope: &str,
    scope_id: &str,
    workspace: &str,
    title: Option<String>,
    text: &str,
) -> R {
    create_draft(
        server,
        ctx,
        &json!({}),
        (scope.into(), scope_id.into(), Some(workspace.into())),
        title,
        bounded_text(text, "text")?,
        vec![],
        vec![],
    )
}

#[allow(clippy::too_many_arguments)]
fn create_draft(
    server: &Server,
    ctx: &Ctx,
    p: &Value,
    (scope, scope_id, workspace): (String, String, Option<String>),
    title: Option<String>,
    text: String,
    attachments: Vec<Attachment>,
    combined_from: Vec<String>,
) -> R {
    let existing = scope_drafts(server, &scope, &scope_id);
    if existing.len() >= MAX_DRAFTS_PER_SCOPE {
        return Err(invalid(format!(
            "at most {MAX_DRAFTS_PER_SCOPE} drafts per {scope}"
        )));
    }
    let order = existing.last().map(|d| d.order + 1.0).unwrap_or(1.0);
    let d = Draft {
        id: crate::core::ulid(),
        scope,
        scope_id,
        workspace,
        title,
        text,
        attachments,
        order,
        rev: 1,
        combined_from,
        sends: vec![],
        archived: false,
        created_at_ms: now(),
        updated_at_ms: now(),
    };
    let mut c = server.core.lock().unwrap();
    if let Some(r) = receipts::replay_in(&c, ctx, "draft.create", p) {
        return r;
    }
    let mut tx = Tx::new();
    put(&mut tx, &d);
    tx.event("draft.created", subject(&d), meta(&d));
    let result = json!({"draft": d});
    receipts::record(&mut tx, ctx, "draft.create", p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(result)
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if !(method.starts_with("draft.") || method.starts_with("notes.")) {
        return None;
    }
    let scope = match caller_ws(server, ctx, method) {
        Ok(s) => s,
        Err(e) => return Some(Err(e)),
    };
    let sc = scope.as_deref();
    Some(match method {
        "draft.create" => create(server, ctx, sc, p),
        "draft.update" => update(server, ctx, sc, p),
        "draft.get" => req(p, "draft")
            .and_then(|id| load(server, sc, id, method))
            .map(|d| json!({"draft": d})),
        "draft.list" => list(server, ctx, sc, p),
        "draft.reorder" => reorder(server, ctx, sc, p),
        "draft.delete" => delete(server, ctx, sc, p),
        "draft.combine" => combine(server, ctx, sc, p),
        "draft.check" => check(server, sc, p).map(|(v, _)| v),
        "draft.send" => send(server, ctx, sc, p).await,
        "draft.reconcile" => reconcile(server, ctx, sc, p),
        "notes.get" => notes_get(server, ctx, sc, p),
        "notes.set" => notes_set(server, ctx, sc, p),
        _ => return None,
    })
}

fn create(server: &Server, ctx: &Ctx, scope: Option<&str>, p: &Value) -> R {
    if let Some(r) = receipts::replay(server, ctx, "draft.create", p) {
        return r;
    }
    let (sk, sid, ws) = resolve_scope(server, ctx, p)?;
    if let Some(my) = scope
        && ws.as_deref() != Some(my)
    {
        return Err(denied("draft.create"));
    }
    let text = bounded_text(s(p, "text").unwrap_or(""), "text")?;
    let att = attachments_from(server, ctx, p)?;
    if text.trim().is_empty() && att.is_empty() {
        return Err(invalid("a draft needs text or an attachment"));
    }
    create_draft(
        server,
        ctx,
        p,
        (sk, sid, ws),
        s(p, "title").map(str::to_string),
        text,
        att,
        vec![],
    )
}

fn update(server: &Server, ctx: &Ctx, scope: Option<&str>, p: &Value) -> R {
    if let Some(r) = receipts::replay(server, ctx, "draft.update", p) {
        return r;
    }
    let id = req(p, "draft")?;
    let mut d = load(server, scope, id, "draft.update")?;
    if let Some(rev) = p.get("expected_rev").and_then(Value::as_u64)
        && rev as u32 != d.rev
    {
        return Err(conflict(
            "draft_changed",
            format!("draft is at revision {}, not {rev}", d.rev),
        ));
    }
    if d.sending() {
        return Err(conflict("send_in_progress", "the draft is being sent"));
    }
    if let Some(t) = s(p, "text") {
        d.text = bounded_text(t, "text")?;
    }
    if let Some(t) = p.get("title") {
        d.title = t.as_str().map(str::to_string);
    }
    if p.get("attachments").is_some() {
        d.attachments = attachments_from(server, ctx, p)?;
    }
    if let Some(a) = p.get("add_attachment") {
        if d.attachments.len() >= MAX_ATTACHMENTS {
            return Err(invalid(format!("at most {MAX_ATTACHMENTS} attachments")));
        }
        d.attachments.push(attachment_from(server, ctx, a)?);
    }
    if let Some(i) = p.get("remove_attachment").and_then(Value::as_u64) {
        if (i as usize) >= d.attachments.len() {
            return Err(invalid("remove_attachment: no attachment at that index"));
        }
        d.attachments.remove(i as usize);
    }
    d.rev += 1;
    d.updated_at_ms = now();
    let mut c = server.core.lock().unwrap();
    let cur = c.store.get::<Draft>(K_DRAFT, &d.id).ok().flatten();
    if cur.as_ref().map(|x| x.rev) != Some(d.rev - 1) {
        return Err(conflict("draft_changed", "the draft changed; reload it"));
    }
    let mut tx = Tx::new();
    put(&mut tx, &d);
    tx.event("draft.updated", subject(&d), meta(&d));
    let result = json!({"draft": d});
    receipts::record(&mut tx, ctx, "draft.update", p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(result)
}

fn list(server: &Server, ctx: &Ctx, scope: Option<&str>, p: &Value) -> R {
    let all = p.get("all").and_then(Value::as_bool) == Some(true) && s(p, "id").is_none();
    let mut v: Vec<Draft> = if all {
        let mut v: Vec<Draft> =
            server.with_core(|c| c.store.load::<Draft>(K_DRAFT).unwrap_or_default());
        v.sort_by(|a, b| {
            (a.scope_id.as_str(), a.order)
                .partial_cmp(&(b.scope_id.as_str(), b.order))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    } else {
        let (sk, sid, ws) = resolve_scope(server, ctx, p)?;
        if let Some(my) = scope
            && ws.as_deref() != Some(my)
        {
            return Err(denied("draft.list"));
        }
        scope_drafts(server, &sk, &sid)
    };
    if let Some(my) = scope {
        v.retain(|d| d.workspace.as_deref() == Some(my));
    }
    // Report restart-interrupted sends as unknown (persisted by `load`).
    let mut out = Vec::with_capacity(v.len());
    for d in v {
        out.push(if d.sending() {
            load(server, None, &d.id, "draft.list")?
        } else {
            d
        });
    }
    Ok(json!({"drafts": out}))
}

fn reorder(server: &Server, ctx: &Ctx, scope: Option<&str>, p: &Value) -> R {
    if let Some(r) = receipts::replay(server, ctx, "draft.reorder", p) {
        return r;
    }
    let ids: Vec<String> = p
        .get("order")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .ok_or_else(|| invalid("`order`: the draft ids in their new order"))?;
    let first = ids.first().ok_or_else(|| invalid("`order` is empty"))?;
    let d0 = load(server, scope, first, "draft.reorder")?;
    let current = scope_drafts(server, &d0.scope, &d0.scope_id);
    for id in &ids {
        if !current.iter().any(|d| &d.id == id) {
            return Err(invalid(format!("draft {id} is not in this {}", d0.scope)));
        }
    }
    // Listed drafts first, in the given order; the rest keep their relative order after.
    let mut ordered: Vec<Draft> = ids
        .iter()
        .filter_map(|id| current.iter().find(|d| &d.id == id).cloned())
        .collect();
    ordered.extend(current.into_iter().filter(|d| !ids.contains(&d.id)));
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    for (i, mut d) in ordered.into_iter().enumerate() {
        let o = (i + 1) as f64;
        if d.order != o {
            d.order = o;
            put(&mut tx, &d);
        }
    }
    tx.event(
        "draft.reordered",
        json!({"scope": d0.scope, "scope_id": d0.scope_id, "workspace": d0.workspace}),
        json!({"count": ids.len()}),
    );
    let result = json!({"order": ids});
    receipts::record(&mut tx, ctx, "draft.reorder", p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(result)
}

fn delete(server: &Server, ctx: &Ctx, scope: Option<&str>, p: &Value) -> R {
    if let Some(r) = receipts::replay(server, ctx, "draft.delete", p) {
        return r;
    }
    let id = req(p, "draft")?;
    let d = load(server, scope, id, "draft.delete")?;
    if d.sending() {
        return Err(conflict("send_in_progress", "the draft is being sent"));
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.delete(K_DRAFT, &d.id);
    tx.event("draft.deleted", subject(&d), json!({"rev": d.rev}));
    let result = json!({"deleted": d.id});
    receipts::record(&mut tx, ctx, "draft.delete", p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(result)
}

fn combine(server: &Server, ctx: &Ctx, scope: Option<&str>, p: &Value) -> R {
    if let Some(r) = receipts::replay(server, ctx, "draft.combine", p) {
        return r;
    }
    let ids: Vec<String> = p
        .get("ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if ids.len() < 2 {
        return Err(invalid("`ids`: at least two drafts to combine"));
    }
    let drafts: Vec<Draft> = ids
        .iter()
        .map(|id| load(server, scope, id, "draft.combine"))
        .collect::<Result<_, _>>()?;
    let (sk, sid, ws) = (
        drafts[0].scope.clone(),
        drafts[0].scope_id.clone(),
        drafts[0].workspace.clone(),
    );
    if drafts.iter().any(|d| d.scope != sk || d.scope_id != sid) {
        return Err(invalid(
            "only drafts of the same workspace or task can be combined",
        ));
    }
    let sep = s(p, "separator").unwrap_or("\n\n");
    let text = bounded_text(
        &drafts
            .iter()
            .map(|d| d.text.trim_end())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(sep),
        "combined text",
    )?;
    let mut att: Vec<Attachment> = vec![];
    for a in drafts.iter().flat_map(|d| d.attachments.iter()) {
        if !att.iter().any(|x| x.path == a.path) {
            att.push(a.clone());
        }
    }
    att.truncate(MAX_ATTACHMENTS);
    let title = s(p, "title")
        .map(str::to_string)
        .or_else(|| drafts[0].title.clone());
    let v = create_draft(server, ctx, p, (sk, sid, ws), title, text, att, ids.clone())?;
    if p.get("delete_sources").and_then(Value::as_bool) == Some(true) {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        for d in drafts.iter().filter(|d| !d.sending()) {
            tx.m.delete(K_DRAFT, &d.id);
            tx.event(
                "draft.deleted",
                subject(d),
                json!({"rev": d.rev, "combined_into": v["draft"]["id"]}),
            );
        }
        server.commit(&mut c, tx).map_err(internal)?;
    }
    Ok(v)
}

// ---- sending --------------------------------------------------------------------------------

/// What `draft.send` would write: the draft text, attachment references as paths, and the
/// workspace notes only when explicitly included.
pub(crate) fn compose(server: &Server, d: &Draft, include_notes: bool) -> String {
    let mut text = d.text.trim_end().to_string();
    if !d.attachments.is_empty() {
        text.push_str("\n\nAttached files:");
        for a in &d.attachments {
            let what = if a.kind == "screenshot" {
                " (screenshot)"
            } else {
                ""
            };
            text.push_str(&format!("\n- {}{what}", a.path));
        }
    }
    if include_notes
        && let Some(ws) = &d.workspace
        && let Some(n) = server.with_core(|c| c.store.get::<Notes>(K_NOTES, ws).ok().flatten())
        && !n.text.trim().is_empty()
    {
        text.push_str("\n\nNotes:\n");
        text.push_str(n.text.trim_end());
    }
    text.trim_start().to_string()
}

struct Target {
    run: AgentRun,
    conversation: Option<String>,
}

/// The send capability for a target run (`send_path: prompt_input | open_pane_only`) and why.
fn check(
    server: &Server,
    scope: Option<&str>,
    p: &Value,
) -> Result<(Value, Option<Target>), RpcError> {
    let rid = s(p, "target_run")
        .or(s(p, "run"))
        .ok_or_else(|| invalid("missing param `target_run`"))?;
    let run = server
        .with_core(|c| c.run(rid).cloned())
        .ok_or_else(|| not_found("run", rid))?;
    let ws = server.with_core(|c| c.pane(&run.pane).map(|x| x.workspace.clone()));
    if let Some(my) = scope
        && ws.as_deref() != Some(my)
    {
        return Err(denied("draft.check"));
    }
    let draft = match s(p, "draft") {
        Some(id) => Some(load(server, scope, id, "draft.check")?),
        None => None,
    };
    let conv = run.harness_session_id.clone();
    let mut why: Vec<String> = vec![];
    match &conv {
        Some(c) => {
            if let Err(e) = crate::tracking::prompt_input_safety(server, &run.id, c) {
                why.push(e);
            }
        }
        None => why.push(
            "the run's native conversation is unknown, so delivery could not be confirmed".into(),
        ),
    }
    let mut hidden = vec![];
    if let Some(d) = &draft {
        for a in &d.attachments {
            let visible = Path::new(&a.path).exists()
                && crate::sandbox::can_see(server, &run.pane, &a.path).unwrap_or(true);
            if !visible {
                hidden.push(a.path.clone());
            }
        }
        if !hidden.is_empty() {
            why.push(format!(
                "the agent can't see attached files: {}",
                hidden.join(", ")
            ));
        }
    }
    let ok = why.is_empty();
    let v = json!({
        "run": run.id, "pane": run.pane, "harness": run.harness,
        "native_conversation_id": conv,
        "send_path": if ok { "prompt_input" } else { "open_pane_only" },
        "unsafe": (!ok).then(|| why.join("; ")),
        "follow_up": "prompt_input",
        "steer": false,
        "hidden_attachments": hidden,
        "note": "prompt_input types into the agent's empty input box only when it is provably safe; otherwise open the pane and send it yourself",
    });
    Ok((
        v,
        Some(Target {
            run,
            conversation: conv,
        }),
    ))
}

async fn send(server: &Arc<Server>, ctx: &Ctx, scope: Option<&str>, p: &Value) -> R {
    const M: &str = "draft.send";
    req(p, "idempotency_key")?;
    let id = req(p, "draft")?.to_string();
    // A repeated key reports the draft as it is now (15 §10.3).
    if let Some(r) = receipts::replay(server, ctx, M, p) {
        return r.and_then(|_| {
            let d = load(server, scope, &id, M)?;
            Ok(json!({"draft": d, "send": d.last_send(), "replayed": true}))
        });
    }
    let d = load(server, scope, &id, M)?;
    if d.archived {
        return Err(conflict(
            "draft_sent",
            "this draft was delivered and archived",
        ));
    }
    let retry = p.get("retry_despite_unknown").and_then(Value::as_bool) == Some(true);
    if let Some(last) = d.last_send() {
        match last.state {
            MessageState::Sending => {
                return Err(conflict("send_in_progress", "this draft is being sent"));
            }
            MessageState::DeliveryUnknown if !last.reconciled => {
                return Err(conflict(
                    "reconcile_first",
                    "the earlier attempt may have arrived: run draft.reconcile before retrying",
                )
                .details(json!({"reason": "reconcile_first", "send": last.id, "idempotency_key": last.idempotency_key})));
            }
            MessageState::DeliveryUnknown if !retry => {
                return Err(conflict(
                    "delivery_unknown",
                    "the earlier attempt may have arrived; pass retry_despite_unknown to send again",
                ));
            }
            MessageState::Delivered if !retry => {
                return Err(conflict(
                    "already_delivered",
                    "this draft was already delivered; pass retry_despite_unknown to send it again",
                ));
            }
            _ => {}
        }
    }
    let include_notes = p.get("include_notes").and_then(Value::as_bool) == Some(true);
    let (cap, target) = check(
        server,
        scope,
        &json!({"target_run": s(p, "target_run").or(s(p, "run")), "draft": id}),
    )?;
    let target = target.expect("check returns its target");
    if cap["send_path"] != "prompt_input" {
        let why = cap["unsafe"].as_str().unwrap_or("").to_string();
        return Err(err(
            ErrorKind::Conflict,
            format!("not sent (zero bytes written): {why}"),
        )
        .details(json!({"reason": "send_unsafe", "detail": why, "fallback": "open_pane_to_send", "pane": target.run.pane, "draft_kept": true})));
    }
    let conv = target.conversation.clone().unwrap_or_default();
    let text = compose(server, &d, include_notes);
    if !server.drafts.inflight.lock().unwrap().insert(d.id.clone()) {
        return Err(conflict("send_in_progress", "this draft is being sent"));
    }
    let release = |srv: &Server, id: &str| {
        srv.drafts.inflight.lock().unwrap().remove(id);
    };
    // Conditional transition under the state lock: only from the revision we validated.
    let (d, send_id) = {
        let mut c = server.core.lock().unwrap();
        if let Some(r) = receipts::replay_in(&c, ctx, M, p) {
            drop(c);
            release(server, &d.id);
            return r;
        }
        let cur = c.store.get::<Draft>(K_DRAFT, &d.id).ok().flatten();
        if cur.as_ref().map(|x| (x.rev, x.sends.len())) != Some((d.rev, d.sends.len())) {
            drop(c);
            release(server, &d.id);
            return Err(conflict("draft_changed", "the draft changed; reload it"));
        }
        let mut d = d;
        let x = DraftSend {
            id: crate::core::ulid(),
            run: target.run.id.clone(),
            pane: target.run.pane.clone(),
            harness: target.run.harness.clone(),
            native_conversation_id: conv.clone(),
            text: text.clone(),
            include_notes,
            state: MessageState::Sending,
            detail: None,
            idempotency_key: s(p, "idempotency_key").unwrap_or("").into(),
            owner: receipts::owner(ctx),
            turn_baseline: target.run.turns_completed + 1,
            reconciled: false,
            created_at_ms: now(),
            updated_at_ms: now(),
        };
        let sid = x.id.clone();
        d.sends.push(x);
        if d.sends.len() > MAX_SENDS_KEPT {
            d.sends.remove(0);
        }
        d.updated_at_ms = now();
        let mut tx = Tx::new();
        put(&mut tx, &d);
        tx.event(
            "draft.sending",
            subject(&d),
            json!({"send": sid, "run": target.run.id, "include_notes": include_notes, "bytes": text.len()}),
        );
        receipts::record(&mut tx, ctx, M, p, &json!({"draft": d.id, "send": sid}));
        if let Err(e) = server.commit(&mut c, tx) {
            drop(c);
            release(server, &d.id);
            return Err(internal(e));
        }
        (d, sid)
    };
    let keep = p.get("keep").and_then(Value::as_bool) == Some(true);
    let srv = server.clone();
    let (did, run_id) = (d.id.clone(), target.run.id.clone());
    tokio::spawn(async move {
        let (state, detail) = match crate::tracking::prompt_input_safety(&srv, &run_id, &conv) {
            Err(why) => (MessageState::Failed, Some(format!("not sent: {why}"))),
            Ok(()) => crate::tracking::deliver_text(&srv, &run_id, &conv, &text).await,
        };
        finish(&srv, &did, &send_id, state, detail, keep);
        release(&srv, &did);
    });
    Ok(json!({"draft": d.id, "send": d.last_send(), "send_path": "prompt_input"}))
}

/// Record a send outcome; a delivered draft is archived unless `keep`.
fn finish(
    server: &Server,
    draft: &str,
    send: &str,
    state: MessageState,
    detail: Option<String>,
    keep: bool,
) {
    let mut c = server.core.lock().unwrap();
    let Some(mut d) = c.store.get::<Draft>(K_DRAFT, draft).ok().flatten() else {
        return;
    };
    let Some(x) = d.sends.iter_mut().find(|x| x.id == send) else {
        return;
    };
    x.state = state;
    x.detail = detail.clone();
    x.updated_at_ms = now();
    if state == MessageState::Delivered && !keep {
        d.archived = true;
    }
    d.updated_at_ms = now();
    let mut tx = Tx::new();
    put(&mut tx, &d);
    let kind = match state {
        MessageState::Delivered => "draft.delivered",
        MessageState::DeliveryUnknown => "draft.delivery_unknown",
        _ => "draft.send_failed",
    };
    tx.event(
        kind,
        subject(&d),
        json!({"send": send, "archived": d.archived}),
    );
    let _ = server.commit(&mut c, tx);
}

/// Inspect the latest attempt before any retry: a late matching turn proves delivery; the
/// caller's receipt for the attempt's key is reported; an attempt still uncertain is marked
/// reconciled, which is what allows `retry_despite_unknown`.
fn reconcile(server: &Server, ctx: &Ctx, scope: Option<&str>, p: &Value) -> R {
    let id = req(p, "draft")?;
    let mut d = load(server, scope, id, "draft.reconcile")?;
    let Some(last) = d.sends.last().cloned() else {
        return Ok(json!({"draft": d, "send": null, "may_retry": true}));
    };
    let receipt = receipts::lookup(server, ctx, &last.idempotency_key)
        .map(|r| json!({"known": true, "method": r.method, "result": r.result, "at_ms": r.at_ms}));
    if last.state == MessageState::Sending {
        return Ok(
            json!({"draft": d, "send": last, "receipt": receipt, "may_retry": false, "note": "still sending"}),
        );
    }
    let mut changed = false;
    if last.state == MessageState::DeliveryUnknown {
        let x = d.sends.last_mut().unwrap();
        if crate::tracking::turn_matches(
            server,
            &last.run,
            &last.native_conversation_id,
            last.turn_baseline,
            &last.text,
        ) {
            x.state = MessageState::Delivered;
            x.detail = Some("a matching turn was found when reconciling".into());
        }
        if !x.reconciled || x.state == MessageState::Delivered {
            x.reconciled = true;
            changed = true;
        }
    }
    if changed {
        let st = d.sends.last().unwrap().state;
        let mut c = server.core.lock().unwrap();
        let cur = c.store.get::<Draft>(K_DRAFT, &d.id).ok().flatten();
        if cur.map(|x| x.sends.len()) != Some(d.sends.len()) {
            return Err(conflict("draft_changed", "the draft changed; reload it"));
        }
        d.updated_at_ms = now();
        let mut tx = Tx::new();
        put(&mut tx, &d);
        tx.event(
            if st == MessageState::Delivered {
                "draft.delivered"
            } else {
                "draft.reconciled"
            },
            subject(&d),
            json!({"send": last.id}),
        );
        server.commit(&mut c, tx).map_err(internal)?;
    }
    let x = d.sends.last().unwrap().clone();
    let may_retry = match x.state {
        MessageState::DeliveryUnknown => x.reconciled,
        MessageState::Failed | MessageState::Cancelled | MessageState::Prepared => true,
        _ => false,
    };
    let note = match x.state {
        MessageState::Delivered => "delivered: a matching turn started in that conversation",
        MessageState::DeliveryUnknown => {
            "still unknown: check the pane; a retry (retry_despite_unknown) may send it twice"
        }
        _ => "not delivered",
    };
    Ok(
        json!({"draft": d, "send": x, "receipt": receipt.unwrap_or(json!({"known": false})), "may_retry": may_retry, "note": note}),
    )
}

// ---- notes ----------------------------------------------------------------------------------

fn notes_ws(
    server: &Server,
    ctx: &Ctx,
    scope: Option<&str>,
    p: &Value,
    m: &str,
) -> Result<String, RpcError> {
    let ws = crate::api::resolve_ws(server, ctx, s(p, "workspace"))?;
    if let Some(my) = scope
        && ws.id != my
    {
        return Err(denied(m));
    }
    Ok(ws.id)
}

fn notes_get(server: &Server, ctx: &Ctx, scope: Option<&str>, p: &Value) -> R {
    let ws = notes_ws(server, ctx, scope, p, "notes.get")?;
    let n = server
        .with_core(|c| c.store.get::<Notes>(K_NOTES, &ws).ok().flatten())
        .unwrap_or(Notes {
            workspace: ws,
            text: String::new(),
            rev: 0,
            updated_at_ms: 0,
        });
    Ok(
        json!({"notes": n, "note": "notes are never sent unless you include them (draft.send --include-notes)"}),
    )
}

fn notes_set(server: &Server, ctx: &Ctx, scope: Option<&str>, p: &Value) -> R {
    if let Some(r) = receipts::replay(server, ctx, "notes.set", p) {
        return r;
    }
    let ws = notes_ws(server, ctx, scope, p, "notes.set")?;
    let text = bounded_text(req(p, "text")?, "notes")?;
    let mut c = server.core.lock().unwrap();
    let cur = c.store.get::<Notes>(K_NOTES, &ws).ok().flatten();
    let rev = cur.as_ref().map(|n| n.rev).unwrap_or(0);
    if let Some(want) = p.get("expected_rev").and_then(Value::as_u64)
        && want as u32 != rev
    {
        return Err(conflict(
            "notes_changed",
            format!("notes are at revision {rev}, not {want}"),
        ));
    }
    let n = Notes {
        workspace: ws.clone(),
        text,
        rev: rev + 1,
        updated_at_ms: now(),
    };
    let mut tx = Tx::new();
    tx.m.put(K_NOTES, &ws, None, &n);
    tx.event(
        "notes.updated",
        json!({"workspace": ws}),
        json!({"rev": n.rev, "bytes": n.text.len()}),
    );
    let result = json!({"notes": n});
    receipts::record(&mut tx, ctx, "notes.set", p, &result);
    server.commit(&mut c, tx).map_err(internal)?;
    Ok(result)
}

/// Retention: archived (delivered) drafts older than [`ARCHIVE_RETENTION_MS`] are removed.
pub fn prune(server: &Server) {
    let cutoff = now() - ARCHIVE_RETENTION_MS;
    let old: Vec<Draft> = server.with_core(|c| {
        c.store
            .load_closed::<Draft>(K_DRAFT, 500)
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d.archived && d.updated_at_ms < cutoff)
            .collect()
    });
    if old.is_empty() {
        return;
    }
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    for d in &old {
        tx.m.delete(K_DRAFT, &d.id);
    }
    let _ = server.commit(&mut c, tx);
}
