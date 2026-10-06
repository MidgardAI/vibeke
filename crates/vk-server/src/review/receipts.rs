//! Caller-owned mutation receipts (15 §6.3, §10.2, §10.3, §11).
//!
//! A receipt belongs to the caller identity that created it: a pane-scoped caller
//! (`pane:<id>`) or the user (`user`, any full-scope client). An owner can replay or query only
//! its own receipts; another owner's key is simply not found, so a pane cannot read a user's
//! results (or vice versa) by guessing keys.
//!
//! Storage is compatible with the existing `op_receipt` rows written by `tracking::record`:
//! user receipts keep the bare idempotency key as their row id (rows without an `owner` were
//! written by user clients — panes cannot call mutating task methods), other owners use
//! `<owner>|<key>`. The payload digest matches `tracking`'s, so either side can replay the
//! other's user receipts.

use crate::Server;
use crate::api::{Ctx, R, s};
use crate::core::{Core, Tx};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const K_RECEIPT: &str = "op_receipt";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedReceipt {
    pub key: String,
    pub method: String,
    pub payload_digest: String,
    pub result: Value,
    pub at_ms: i64,
    /// `user` or `pane:<id>`; `None` on rows written before ownership was recorded (user).
    #[serde(default)]
    pub owner: Option<String>,
}

impl OwnedReceipt {
    pub fn owner(&self) -> &str {
        self.owner.as_deref().unwrap_or("user")
    }
}

/// The receipt owner for a caller: `pane:<id>` for pane-token callers, else `user`.
pub fn owner(ctx: &Ctx) -> String {
    match &ctx.pane_scope {
        Some(p) => format!("pane:{p}"),
        None => "user".into(),
    }
}

fn row_id(owner: &str, key: &str) -> String {
    if owner == "user" {
        key.to_string()
    } else {
        format!("{owner}|{key}")
    }
}

/// Request digest without the idempotency key (same algorithm as `tracking`).
pub fn digest(method: &str, p: &Value) -> String {
    let mut p = p.clone();
    if let Some(o) = p.as_object_mut() {
        o.remove("idempotency_key");
    }
    blake3::hash(format!("{method}\n{p}").as_bytes()).to_hex()[..32].to_string()
}

/// The caller's own receipt for `key`, if any (call with the core lock held).
pub fn get_in(c: &Core, ctx: &Ctx, key: &str) -> Option<OwnedReceipt> {
    let o = owner(ctx);
    let r = c
        .store
        .get::<OwnedReceipt>(K_RECEIPT, &row_id(&o, key))
        .ok()
        .flatten()?;
    (r.owner() == o).then_some(r)
}

/// The caller's own receipt for `key`, if any. Intended for `task.operation.get`.
pub fn lookup(server: &Server, ctx: &Ctx, key: &str) -> Option<OwnedReceipt> {
    server.with_core(|c| get_in(c, ctx, key))
}

fn replay_of(r: OwnedReceipt, method: &str, p: &Value) -> R {
    if r.method != method || r.payload_digest != digest(method, p) {
        return Err(crate::tracking::conflict(
            "idempotency_key_reused",
            "this idempotency key was used for a different request",
        ));
    }
    let mut v = r.result;
    v["replayed"] = json!(true);
    Ok(v)
}

/// A previous outcome of this caller's key (call with the core lock held): the replayed
/// result, or a conflict when the key was used for a different request.
pub fn replay_in(c: &Core, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    let key = s(p, "idempotency_key")?;
    get_in(c, ctx, key).map(|r| replay_of(r, method, p))
}

/// [`replay_in`] taking the lock itself (fast path before expensive work; recheck with
/// [`replay_in`] inside the committing transaction).
pub fn replay(server: &Server, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    server.with_core(|c| replay_in(c, ctx, method, p))
}

/// Record this caller's receipt in the same transaction as the mutation.
pub fn record(tx: &mut Tx, ctx: &Ctx, method: &str, p: &Value, result: &Value) {
    let Some(key) = s(p, "idempotency_key") else {
        return;
    };
    let o = owner(ctx);
    let r = OwnedReceipt {
        key: key.into(),
        method: method.into(),
        payload_digest: digest(method, p),
        result: result.clone(),
        at_ms: vk_store::now_ms(),
        owner: Some(o.clone()),
    };
    tx.m.close(K_RECEIPT, &row_id(&o, key), None, &r);
}
