//! Mentions (spec 19): pane names (`@reviewer`) and messages between the panes of a workspace.
//!
//! - [`names`]: one mention name per pane, unique in its workspace, kept in
//!   `SessionModel.mentions` (render protocol 10) and the store (`core::K_MENTION`). A name
//!   goes away with its pane (`Core::commit`).
//! - Messages and deliveries are store entities, not part of the render model. Bodies are
//!   redacted before they are stored; events carry ids and states, never bodies (§3).
//! - [`delivery`]: the engine that types a one-line envelope into an idle recipient (§4.1–4.2).
//! - [`hooks`]: what the Claude and Codex hooks add to the `adapter.signal` reply (§4.3–4.5).

pub mod delivery;
pub mod hooks;
pub mod names;

pub use delivery::start;

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, not_found, req, resolve_pane, s, u};
use crate::core::{Core, Tx};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use vk_proto::mention::{self as vm, Author, Delivery, DeliveryState, MentionKind, Message};
use vk_proto::model::Pane;
use vk_proto::rpc::{ErrorKind, RpcError};
use vk_store::now_ms;

pub const K_MESSAGE: &str = "mention_message";
pub const K_DELIVERY: &str = "mention_delivery";

pub const METHODS: &[(&str, bool)] = &[
    ("mention.list", false),
    ("mention.suggest", false),
    ("mention.set", true),
    ("mention.clear", true),
    ("message.send", true),
    ("message.reply", true),
    ("message.complete", true),
    ("message.get", false),
    ("message.list", false),
    ("message.cancel", true),
    ("message.retry", true),
];

/// Retrying a held delivery is the person's decision (§4.6).
pub const PANE_FORBIDDEN: &[&str] = &["message.retry"];

/// Methods a pane-scoped caller may use on its own pane only (`api::pane_scope_of`): an agent
/// names only its own pane (§2.3) and reads only its own messages.
pub const OWN_PANE: &[&str] = &["mention.set", "mention.clear", "message.list"];

/// Largest message body, before redaction (§4.6).
pub const MAX_BODY: usize = 64 * 1024;
/// Messages one pane may send per minute (§4.6).
pub const RATE_PER_MIN: usize = 20;

#[derive(Default)]
pub struct State {
    /// Send times per sending pane over the last minute (rate limit).
    sent: Mutex<HashMap<String, VecDeque<i64>>>,
    /// Deliveries whose envelope (or fallback body) is being typed by this process.
    typing: Mutex<std::collections::HashSet<String>>,
    /// Whether the store may hold open deliveries: the engine skips its pass otherwise.
    open: AtomicBool,
    /// Wakes the delivery engine (a new delivery, a name change, a retry).
    pub wake: Notify,
}

impl State {
    pub(crate) fn poke(&self) {
        self.open.store(true, Ordering::Relaxed);
        self.wake.notify_one();
    }
}

/// A delivery as stored: the public [`Delivery`] and the engine's bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DeliveryRec {
    pub delivery: Delivery,
    /// The envelope secret (`Delivery.nonce` is never serialized, so it is kept here).
    pub nonce: String,
    pub workspace: String,
    pub queued_at_ms: i64,
    #[serde(default)]
    pub submitted_at_ms: Option<i64>,
    /// Envelopes the recipient refused (a dialog, no run): held after two.
    #[serde(default)]
    pub refusals: u32,
    /// The Stop hook already reminded the recipient to answer.
    #[serde(default)]
    pub reminded: bool,
    /// The framed body was typed because the harness has no mention hooks.
    #[serde(default)]
    pub fallback: bool,
    /// The recipient is the author of the message this one answers: it needs no name.
    #[serde(default)]
    pub reply_route: bool,
}

pub(crate) fn author_pane(a: &Author) -> Option<&str> {
    match a {
        Author::Human { pane } => pane.as_deref(),
        Author::Agent { pane, .. } => Some(pane),
    }
}

fn now() -> i64 {
    now_ms()
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", crate::core::ulid().to_ascii_lowercase())
}

/// 12 random base32 characters (§4.1).
fn new_nonce() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    (0..12)
        .map(|_| ALPHABET[(rand::random::<u8>() & 31) as usize] as char)
        .collect()
}

fn denied(msg: impl Into<String>) -> RpcError {
    err(ErrorKind::PermissionDenied, msg).details(json!({"scope": "pane"}))
}

fn conflict(reason: &str, msg: impl Into<String>) -> RpcError {
    err(ErrorKind::Conflict, msg).details(json!({"reason": reason}))
}

pub(crate) fn put_delivery(tx: &mut Tx, r: &DeliveryRec) {
    if r.delivery.state.is_open() {
        tx.m.put(K_DELIVERY, &r.delivery.id, None, r);
    } else {
        tx.m.close(K_DELIVERY, &r.delivery.id, None, r);
    }
}

pub(crate) fn delivery_event(tx: &mut Tx, r: &DeliveryRec) {
    let d = &r.delivery;
    tx.event(
        "message.delivery",
        json!({"delivery": d.id, "message": d.message, "pane": d.pane, "workspace": r.workspace}),
        json!({"delivery": d.id, "message": d.message, "name": d.name, "state": d.state, "reason": d.reason}),
    );
}

/// Change one delivery and commit it, with a `message.delivery` event when its state or reason
/// changed. `f` returns false to leave it alone. Returns the delivery as committed.
pub(crate) fn update(
    server: &Server,
    id: &str,
    f: impl FnOnce(&mut DeliveryRec) -> bool,
) -> Option<DeliveryRec> {
    let mut c = server.core.lock().unwrap();
    let mut rec: DeliveryRec = c.store.get(K_DELIVERY, id).ok().flatten()?;
    let before = (rec.delivery.state, rec.delivery.reason.clone());
    if !f(&mut rec) {
        return None;
    }
    rec.delivery.updated_at_ms = now();
    let mut tx = Tx::new();
    put_delivery(&mut tx, &rec);
    if (rec.delivery.state, rec.delivery.reason.clone()) != before {
        delivery_event(&mut tx, &rec);
    }
    server.commit(&mut c, tx).ok()?;
    Some(rec)
}

pub(crate) fn load_message(c: &Core, id: &str) -> Option<Message> {
    c.store.get::<Message>(K_MESSAGE, id).ok().flatten()
}

pub(crate) fn deliveries_of(c: &Core, message: &str) -> Vec<DeliveryRec> {
    c.store
        .load_by_field::<DeliveryRec>(K_DELIVERY, "$.delivery.message", message)
        .unwrap_or_default()
}

pub(crate) fn deliveries_to(c: &Core, pane: &str) -> Vec<DeliveryRec> {
    c.store
        .load_by_field::<DeliveryRec>(K_DELIVERY, "$.delivery.pane", pane)
        .unwrap_or_default()
}

/// Open deliveries, oldest first.
pub(crate) fn open_deliveries(c: &Core) -> Vec<DeliveryRec> {
    let mut v: Vec<DeliveryRec> = c.store.load(K_DELIVERY).unwrap_or_default();
    v.sort_by_key(|r| r.queued_at_ms);
    v
}

fn item(m: &Message, ds: &[DeliveryRec]) -> Value {
    json!({"message": m, "deliveries": ds.iter().map(|r| &r.delivery).collect::<Vec<_>>()})
}

/// The body as stored: bounded, not empty, redacted (§4.6).
fn check_body(body: &str) -> Result<String, RpcError> {
    if body.trim().is_empty() {
        return Err(invalid("the message body is empty"));
    }
    if body.len() > MAX_BODY {
        return Err(invalid(format!(
            "the message body is larger than {} KiB",
            MAX_BODY / 1024
        )));
    }
    Ok(vk_redact::redact(body).into_owned())
}

/// At most [`RATE_PER_MIN`] messages a minute from one pane (§4.6).
fn rate_limit(server: &Server, pane: Option<&str>) -> Result<(), RpcError> {
    let Some(pane) = pane else { return Ok(()) };
    let now = now();
    let mut g = server.mentions.sent.lock().unwrap();
    let q = g.entry(pane.to_string()).or_default();
    while q.front().is_some_and(|t| now - t >= 60_000) {
        q.pop_front();
    }
    if q.len() >= RATE_PER_MIN {
        return Err(err(
            ErrorKind::RateLimited,
            format!("rate_limited: at most {RATE_PER_MIN} messages a minute from one pane"),
        )
        .details(json!({"reason": "rate_limited", "per_minute": RATE_PER_MIN})));
    }
    q.push_back(now);
    Ok(())
}

/// The pane a pane-scoped caller acts for, or the `pane` param of a person's call.
fn caller_pane(server: &Server, ctx: &Ctx, p: &Value, method: &str) -> Result<Pane, RpcError> {
    match &ctx.pane_scope {
        Some(own) => {
            let pane = resolve_pane(server, ctx, s(p, "pane").or(Some("@current")))?;
            if &pane.id != own {
                return Err(denied(format!("{method}: only for the caller's own pane")));
            }
            Ok(pane)
        }
        None => match s(p, "pane") {
            Some(t) => resolve_pane(server, ctx, Some(t)),
            None => Err(invalid(format!("{method} needs `pane` (or a pane token)"))),
        },
    }
}

/// How deep the sender's current work is in a chain of messages: a pane working on a delivered
/// message passes its hop on to what it sends (loop guard, §4.6).
fn hop_of_sender(c: &Core, pane: Option<&str>) -> u32 {
    let Some(pane) = pane else { return 0 };
    deliveries_to(c, pane)
        .iter()
        .filter(|r| r.delivery.state == DeliveryState::Delivered)
        .filter_map(|r| load_message(c, &r.delivery.message))
        .map(|m| m.hop + 1)
        .max()
        .unwrap_or(0)
}

/// A new message to named panes of `workspace` (`message.send`, a leading mention).
pub(crate) struct Outgoing<'a> {
    pub workspace: String,
    pub from: Author,
    pub to: Vec<String>,
    pub body: &'a str,
    pub expects_reply: bool,
}

pub(crate) fn send(server: &Server, o: Outgoing) -> R {
    let body = check_body(o.body)?;
    let mut to: Vec<String> = Vec::new();
    for n in &o.to {
        let n = n.trim().trim_start_matches('@').to_string();
        if !vm::valid(&n) {
            return Err(invalid(format!(
                "`{n}` is not a mention name ([a-z][a-z0-9_-]{{0,31}}, not ending with - or _, not reserved)"
            )));
        }
        if !to.contains(&n) {
            to.push(n);
        }
    }
    if to.is_empty() {
        return Err(invalid("message.send needs at least one name in `to`"));
    }
    let from_pane = author_pane(&o.from).map(str::to_string);
    let mut c = server.core.lock().unwrap();
    let in_ws: Vec<vm::MentionName> = c
        .model
        .mentions
        .iter()
        .filter(|m| m.workspace == o.workspace)
        .cloned()
        .collect();
    let unknown: Vec<&String> = to
        .iter()
        .filter(|n| !in_ws.iter().any(|m| &m.name == *n))
        .collect();
    if !unknown.is_empty() {
        let known: Vec<String> = in_ws.iter().map(|m| format!("@{}", m.name)).collect();
        let list: Vec<String> = unknown.iter().map(|n| format!("@{n}")).collect();
        return Err(err(
            ErrorKind::NotFound,
            format!(
                "no pane named {} in this workspace (named panes: {})",
                list.join(", "),
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            ),
        )
        .details(json!({"object": "mention", "unknown": list, "known": known})));
    }
    let targets: Vec<&vm::MentionName> = to
        .iter()
        .filter_map(|n| in_ws.iter().find(|m| &m.name == n))
        .collect();
    for t in &targets {
        if from_pane.as_deref() == Some(t.pane.as_str()) {
            return Err(invalid(format!("@{} is the sender's own pane", t.name)));
        }
        if t.kind == MentionKind::Terminal {
            return Err(err(
                ErrorKind::PermissionDenied,
                format!("@{} is a terminal; messages go to agent panes only", t.name),
            )
            .details(json!({"reason": "terminal_target", "name": t.name})));
        }
    }
    let hop = match &o.from {
        Author::Agent { .. } => hop_of_sender(&c, from_pane.as_deref()),
        Author::Human { .. } => 0,
    };
    if hop > vm::MAX_HOPS {
        return Err(conflict(
            "hop_limit",
            format!(
                "hop_limit: a chain of messages may be at most {} deep",
                vm::MAX_HOPS
            ),
        ));
    }
    rate_limit(server, from_pane.as_deref())?;
    let id = new_id("m");
    let t0 = now();
    let message = Message {
        id: id.clone(),
        thread: id.clone(),
        workspace: o.workspace.clone(),
        from: o.from,
        to: to.clone(),
        body,
        in_reply_to: None,
        hop,
        sent_at_ms: t0,
    };
    let recs: Vec<DeliveryRec> = targets
        .iter()
        .map(|t| new_delivery(&message, &t.name, &t.pane, o.expects_reply, false))
        .collect();
    let mut tx = Tx::new();
    record(&mut tx, &message, &recs);
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    server.mentions.poke();
    Ok(item(&message, &recs))
}

fn new_delivery(
    m: &Message,
    name: &str,
    pane: &str,
    expects_reply: bool,
    reply_route: bool,
) -> DeliveryRec {
    let nonce = new_nonce();
    DeliveryRec {
        delivery: Delivery {
            id: new_id("d"),
            message: m.id.clone(),
            name: name.to_string(),
            pane: pane.to_string(),
            state: DeliveryState::Queued,
            reason: None,
            nonce: nonce.clone(),
            expects_reply,
            reply: None,
            updated_at_ms: m.sent_at_ms,
        },
        nonce,
        workspace: m.workspace.clone(),
        queued_at_ms: m.sent_at_ms,
        submitted_at_ms: None,
        refusals: 0,
        reminded: false,
        fallback: false,
        reply_route,
    }
}

/// Store a new message with its deliveries and the events (ids and states, never the body).
fn record(tx: &mut Tx, m: &Message, recs: &[DeliveryRec]) {
    tx.m.put(K_MESSAGE, &m.id, None, m);
    let (kind, pane) = match &m.from {
        Author::Human { pane } => ("human", pane.clone()),
        Author::Agent { pane, .. } => ("agent", Some(pane.clone())),
    };
    tx.event(
        "message.sent",
        json!({"message": m.id, "thread": m.thread, "workspace": m.workspace}),
        json!({
            "from": {"kind": kind, "pane": pane},
            "to": m.to,
            "hop": m.hop,
            "in_reply_to": m.in_reply_to,
            "bytes": m.body.len(),
            "deliveries": recs.iter().map(|r| r.delivery.id.clone()).collect::<Vec<_>>(),
        }),
    );
    for r in recs {
        put_delivery(tx, r);
        delivery_event(tx, r);
    }
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if !(method.starts_with("mention.") || method.starts_with("message.")) {
        return None;
    }
    Some(match method {
        "mention.list" => names::list(server, ctx, p),
        "mention.suggest" => names::suggest(server, ctx, p),
        "mention.set" => names::set(server, ctx, p),
        "mention.clear" => names::clear(server, ctx, p),
        "message.send" => send_api(server, ctx, p),
        "message.reply" => reply(server, ctx, p),
        "message.complete" => complete(server, ctx, p),
        "message.get" => get(server, ctx, p),
        "message.list" => list(server, ctx, p),
        "message.cancel" => cancel(server, ctx, p),
        "message.retry" => retry(server, ctx, p),
        _ => return None,
    })
}

fn send_api(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let to: Vec<String> = match p.get("to") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(one)) => vec![one.clone()],
        _ => return Err(invalid("missing param `to` (a list of names)")),
    };
    let body = req(p, "body")?;
    let expects_reply = b(p, "expects_reply").unwrap_or(true);
    let (workspace, from) = match &ctx.pane_scope {
        Some(own) => {
            let (ws, name) = server.with_core(|c| {
                (
                    c.pane(own).map(|p| p.workspace.clone()),
                    c.model.mention_of(own).map(|m| m.name.clone()),
                )
            });
            let ws = ws.ok_or_else(|| denied("message.send: the caller's pane is gone"))?;
            if let Some(w) = s(p, "workspace") {
                let target = crate::api::resolve_ws(server, ctx, Some(w))?;
                if target.id != ws {
                    return Err(denied("messages stay inside the sender's workspace"));
                }
            }
            (
                ws,
                Author::Agent {
                    pane: own.clone(),
                    name,
                },
            )
        }
        None => {
            let ws = match s(p, "workspace") {
                Some(w) => crate::api::resolve_ws(server, ctx, Some(w))?.id,
                None => workspace_of_names(server, &to)?,
            };
            (ws, Author::Human { pane: None })
        }
    };
    send(
        server,
        Outgoing {
            workspace,
            from,
            to,
            body,
            expects_reply,
        },
    )
}

/// A person's send without `workspace`: the one workspace where the first name is unique
/// (§3.2), else the focused workspace when the name is unknown everywhere.
fn workspace_of_names(server: &Server, to: &[String]) -> Result<String, RpcError> {
    let first = to
        .first()
        .map(|n| n.trim().trim_start_matches('@').to_string())
        .unwrap_or_default();
    let hits: Vec<String> = server.with_core(|c| {
        c.model
            .mentions
            .iter()
            .filter(|m| m.name == first)
            .map(|m| m.workspace.clone())
            .collect()
    });
    match hits.as_slice() {
        [ws] => Ok(ws.clone()),
        [] => server
            .with_core(|c| c.model.workspaces.first().map(|w| w.id.clone()))
            .ok_or_else(|| not_found("mention", &format!("@{first}"))),
        _ => Err(err(
            ErrorKind::AmbiguousTarget,
            format!("@{first} names panes in several workspaces; pass `workspace`"),
        )
        .details(json!({"target": format!("@{first}"), "workspaces": hits}))),
    }
}

fn reply(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let mid = req(p, "message")?;
    let body = check_body(req(p, "body")?)?;
    let pane = caller_pane(server, ctx, p, "message.reply")?;
    let mut c = server.core.lock().unwrap();
    let orig = load_message(&c, mid).ok_or_else(|| not_found("message", mid))?;
    let Some(mut answered) = deliveries_of(&c, &orig.id)
        .into_iter()
        .find(|r| r.delivery.pane == pane.id)
    else {
        return Err(denied(format!(
            "message.reply: this pane did not receive {}",
            orig.id
        )));
    };
    let hop = orig.hop + 1;
    if hop > vm::MAX_HOPS {
        return Err(conflict(
            "hop_limit",
            format!(
                "hop_limit: a chain of messages may be at most {} deep",
                vm::MAX_HOPS
            ),
        ));
    }
    rate_limit(server, Some(pane.id.as_str()))?;
    let my_name = c.model.mention_of(&pane.id).map(|m| m.name.clone());
    // The reply goes to the original author (§4.5): an agent or a person's pane gets an
    // envelope, a person without a pane a notification.
    let (to_label, target): (String, Option<(String, String)>) = match &orig.from {
        Author::Agent { pane: ap, name } => {
            let label = c
                .model
                .mention_of(ap)
                .map(|m| m.name.clone())
                .or(name.clone())
                .or_else(|| c.pane(ap).map(|p| p.handle.clone()))
                .unwrap_or_else(|| ap.clone());
            let live = c.pane(ap).is_some();
            (label.clone(), live.then(|| (label, ap.clone())))
        }
        Author::Human { pane: Some(hp) } => {
            let label = c
                .model
                .mention_of(hp)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| "human".to_string());
            let live = c.pane(hp).is_some();
            (label.clone(), live.then(|| (label, hp.clone())))
        }
        Author::Human { pane: None } => ("human".to_string(), None),
    };
    let id = new_id("m");
    let message = Message {
        id: id.clone(),
        thread: orig.thread.clone(),
        workspace: orig.workspace.clone(),
        from: Author::Agent {
            pane: pane.id.clone(),
            name: my_name.clone(),
        },
        to: vec![to_label],
        body,
        in_reply_to: Some(orig.id.clone()),
        hop,
        sent_at_ms: now(),
    };
    let recs: Vec<DeliveryRec> = target
        .iter()
        .map(|(name, tp)| new_delivery(&message, name, tp, false, true))
        .collect();
    let mut tx = Tx::new();
    record(&mut tx, &message, &recs);
    if answered.delivery.state != DeliveryState::Cancelled {
        answered.delivery.state = DeliveryState::Replied;
        answered.delivery.reason = None;
    }
    answered.delivery.reply = Some(id.clone());
    answered.delivery.updated_at_ms = now();
    put_delivery(&mut tx, &answered);
    delivery_event(&mut tx, &answered);
    server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    if matches!(orig.from, Author::Human { pane: None }) {
        let who = my_name
            .map(|n| format!("@{n}"))
            .unwrap_or_else(|| pane.handle.clone());
        server.notify(
            "message",
            Some(&pane.id),
            &format!("{who} replied"),
            &format!(
                "Reply {id} to your message {}. `vibeke msg list` shows it.",
                orig.id
            ),
            "normal",
        );
    }
    server.mentions.poke();
    Ok(item(&message, &recs))
}

fn complete(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let mid = req(p, "message")?;
    let pane = caller_pane(server, ctx, p, "message.complete")?;
    let rec = server
        .with_core(|c| {
            deliveries_of(c, mid)
                .into_iter()
                .find(|r| r.delivery.pane == pane.id)
        })
        .ok_or_else(|| denied(format!("message.complete: this pane did not receive {mid}")))?;
    if rec.delivery.state.is_open() {
        update(server, &rec.delivery.id, |r| {
            if !r.delivery.state.is_open() {
                return false;
            }
            r.delivery.state = DeliveryState::Completed;
            r.delivery.reason = None;
            true
        });
    }
    server.mentions.poke();
    Ok(json!({}))
}

/// Whether a pane-scoped caller may read a message: its sender or one of its recipients.
fn may_read(ctx: &Ctx, m: &Message, ds: &[DeliveryRec]) -> bool {
    match &ctx.pane_scope {
        None => true,
        Some(own) => {
            author_pane(&m.from) == Some(own.as_str()) || ds.iter().any(|r| &r.delivery.pane == own)
        }
    }
}

fn get(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let id = s(p, "id")
        .or(s(p, "message"))
        .ok_or_else(|| invalid("missing param `id`"))?;
    let (m, ds) = server
        .with_core(|c| {
            load_message(c, id).map(|m| {
                let ds = deliveries_of(c, &m.id);
                (m, ds)
            })
        })
        .ok_or_else(|| not_found("message", id))?;
    if !may_read(ctx, &m, &ds) {
        return Err(denied("message.get: only its sender and recipients"));
    }
    Ok(item(&m, &ds))
}

fn list(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let pane = match (&ctx.pane_scope, s(p, "pane")) {
        (Some(_), _) => Some(caller_pane(server, ctx, p, "message.list")?),
        (None, Some(t)) => Some(resolve_pane(server, ctx, Some(t))?),
        (None, None) => None,
    };
    let direction = s(p, "direction").unwrap_or("all");
    if !matches!(direction, "in" | "out" | "all") {
        return Err(invalid("direction: expected in, out or all"));
    }
    let open_only = b(p, "open_only").unwrap_or(false);
    let limit = u(p, "limit").unwrap_or(50).clamp(1, 500) as usize;
    let mut items: Vec<(Message, Vec<DeliveryRec>)> = server.with_core(|c| {
        let mut ids: Vec<String> = Vec::new();
        match &pane {
            Some(pane) => {
                if direction != "out" {
                    ids.extend(
                        deliveries_to(c, &pane.id)
                            .into_iter()
                            .map(|r| r.delivery.message),
                    );
                }
                if direction != "in" {
                    ids.extend(
                        c.store
                            .load_by_field::<Message>(K_MESSAGE, "$.from.pane", &pane.id)
                            .unwrap_or_default()
                            .into_iter()
                            .map(|m| m.id),
                    );
                }
            }
            None => ids.extend(
                c.store
                    .load::<Message>(K_MESSAGE)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|m| m.id),
            ),
        }
        let mut seen = std::collections::HashSet::new();
        ids.retain(|i| seen.insert(i.clone()));
        ids.into_iter()
            .filter_map(|id| load_message(c, &id))
            .map(|m| {
                let ds = deliveries_of(c, &m.id);
                (m, ds)
            })
            .collect()
    });
    if open_only {
        items.retain(|(_, ds)| ds.iter().any(|r| r.delivery.state.is_open()));
    }
    items.sort_by(|a, b| b.0.sent_at_ms.cmp(&a.0.sent_at_ms));
    items.truncate(limit);
    Ok(json!({"items": items.iter().map(|(m, ds)| item(m, ds)).collect::<Vec<_>>()}))
}

fn cancel(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let id = req(p, "delivery")?;
    let (rec, m) = server
        .with_core(|c| {
            let rec: DeliveryRec = c.store.get(K_DELIVERY, id).ok().flatten()?;
            let m = load_message(c, &rec.delivery.message)?;
            Some((rec, m))
        })
        .ok_or_else(|| not_found("delivery", id))?;
    if let Some(own) = &ctx.pane_scope
        && author_pane(&m.from) != Some(own.as_str())
    {
        return Err(denied("message.cancel: only the sender cancels a delivery"));
    }
    if !rec.delivery.state.is_open() {
        return Err(conflict(
            "not_open",
            format!("the delivery is already {:?}", rec.delivery.state).to_lowercase(),
        ));
    }
    update(server, id, |r| {
        if !r.delivery.state.is_open() {
            return false;
        }
        r.delivery.state = DeliveryState::Cancelled;
        r.delivery.reason = None;
        true
    });
    Ok(json!({}))
}

fn retry(server: &Server, _ctx: &Ctx, p: &Value) -> R {
    let id = req(p, "delivery")?;
    let rec: DeliveryRec = server
        .with_core(|c| c.store.get(K_DELIVERY, id).ok().flatten())
        .ok_or_else(|| not_found("delivery", id))?;
    if !matches!(
        rec.delivery.state,
        DeliveryState::Held | DeliveryState::Failed | DeliveryState::Queued
    ) {
        return Err(conflict(
            "not_held",
            "only a held, failed or queued delivery can be retried",
        ));
    }
    update(server, id, |r| {
        r.delivery.state = DeliveryState::Queued;
        r.delivery.reason = None;
        r.queued_at_ms = now();
        r.submitted_at_ms = None;
        r.refusals = 0;
        r.fallback = false;
        true
    });
    server.mentions.poke();
    Ok(json!({}))
}

/// Shapes for the API schema registry (`api_schema`).
pub const DEFS: &str = r##"
MentionName = {name: string, workspace: string, pane: string, kind: agent|terminal, delivery: bool, created_at_ms: int}
MentionEntry = {name: string, workspace: string, pane: string, kind: agent|terminal, delivery: bool, created_at_ms: int, pane_handle: string|null, label: string, harness: string|null, state: idle|busy|needs_input|exited|shell}
MentionAuthor = {kind: human|agent, pane: string|null, name?: string|null}
MentionMessage = {id: string, thread: string, workspace: string, from: MentionAuthor, to: [string], body: string, in_reply_to: string|null, hop: int, sent_at_ms: int}
MentionDelivery = {id: string, message: string, name: string, pane: string, state: queued|submitted|delivered|replied|completed|held|failed|cancelled, reason: string|null, expects_reply: bool, reply: string|null, updated_at_ms: int}
MentionThreadItem = {message: MentionMessage, deliveries: [MentionDelivery]}
"##;

pub const SHAPES: &str = r##"
# --- mentions (spec 19): pane names and messages between the panes of a workspace ---
# the named panes of a workspace (default: the caller's workspace, else all); `unnamed: true` adds the other panes with a suggested name
mention.list :: {workspace?: Target, unnamed?: bool = false} => {mentions: [MentionEntry], unnamed?: [{pane: string, pane_handle: string, workspace: string, suggested: string, label: string, harness: string|null, state: idle|busy|needs_input|exited|shell}]}
# the pane's name, or a proposal built from its sidebar label (§2.2)
mention.suggest :: {pane?: Target} => {name: string, named: bool}
# a pane-scoped caller names only its own pane; name omitted: the current or the suggested name
mention.set :: {pane?: Target, name?: string, kind?: agent|terminal, delivery?: bool} => MentionName
mention.clear :: {pane?: Target} => {}
# to named panes of one workspace; a pane-scoped caller sends from its own pane inside its own workspace
message.send :: {to: [string], body: string, expects_reply?: bool = true, workspace?: Target} => MentionThreadItem
# by a recipient pane of `message`; goes back to its author (an envelope, or a notification for a person without a pane)
message.reply :: {message: string, body: string, pane?: Target} => MentionThreadItem
message.complete :: {message: string, pane?: Target} => {}
message.get :: {id: string} => MentionThreadItem
message.list :: {pane?: Target, direction?: in|out|all = all, open_only?: bool = false, limit?: int = 50} => {items: [MentionThreadItem]}
# the sender's pane, or a person
message.cancel :: {delivery: string} => {}
# full scope only: a held, failed or queued delivery goes back to the queue
message.retry :: {delivery: string} => {}
"##;

pub const EVENTS: &str = r##"
mention.set :: {pane: string, workspace: string} => {name: string, kind: agent|terminal, delivery: bool, previous: string|null, via: string}
mention.cleared :: {pane: string, workspace: string} => {name: string, reason: cleared|pane_closed|pane_moved}
message.sent :: {message: string, thread: string, workspace: string} => {from: {kind: human|agent, pane: string|null}, to: [string], hop: int, in_reply_to: string|null, bytes: int, deliveries: [string]}
message.delivery :: {delivery: string, message: string, pane: string, workspace: string} => {delivery: string, message: string, name: string, state: queued|submitted|delivered|replied|completed|held|failed|cancelled, reason: string|null}
"##;

#[cfg(test)]
#[path = "mentions_tests.rs"]
mod tests;
