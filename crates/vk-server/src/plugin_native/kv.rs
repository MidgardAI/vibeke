//! `plugin.kv.*` (07 §7.5): plugin-only, scoped to the caller's plugin id, stored in the
//! session's `state.db` (`plugin_kv`). Values are JSON (`value`) or bytes (`value_b64`); each
//! is at most 1 MiB and a plugin holds at most `[plugins] kv_quota_bytes` (64 MiB) in total.
//!
//! | Method | Params → Result |
//! |---|---|
//! | `plugin.kv.get` | `{key}` → `{key, value?, value_b64?, found}` |
//! | `plugin.kv.set` | `{key, value \| value_b64}` → `{key, bytes, used, quota}` |
//! | `plugin.kv.delete` | `{key}` → `{key, deleted}` |
//! | `plugin.kv.list` | `{prefix?, after?, limit? ≤ 1000}` → `{keys: [{key, bytes}], next?}` |

use crate::Server;
use crate::api::{Ctx, R, err, internal, invalid};
use base64::Engine;
use serde_json::{Value, json};
use vk_proto::rpc::ErrorKind;

/// Stored values carry a one-byte tag: JSON text or raw bytes.
const TAG_JSON: u8 = b'j';
const TAG_BYTES: u8 = b'b';

pub fn quota(plugin: &str) -> u64 {
    super::setting(plugin, "kv_quota_bytes")
        .and_then(|v| v.as_integer())
        .map(|n| n.clamp(1024, 1 << 34) as u64)
        .unwrap_or(vk_store::PLUGIN_KV_QUOTA)
}

pub fn api(server: &Server, ctx: &Ctx, method: &str, p: &Value) -> R {
    let info = super::require_plugin(server, ctx, method)?;
    let plugin = info.plugin.as_str();
    let key = || {
        p.get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("key is required"))
    };
    match method {
        "plugin.kv.get" => {
            let k = key()?;
            let v = server
                .with_core(|c| c.store.plugin_kv_get(plugin, k))
                .map_err(internal)?;
            Ok(match v {
                None => json!({"key": k, "found": false}),
                Some(b) if b.first() == Some(&TAG_JSON) => {
                    let val: Value = serde_json::from_slice(&b[1..]).unwrap_or(Value::Null);
                    json!({"key": k, "found": true, "value": val})
                }
                Some(b) => json!({
                    "key": k,
                    "found": true,
                    "value_b64": base64::engine::general_purpose::STANDARD.encode(b.get(1..).unwrap_or_default()),
                }),
            })
        }
        "plugin.kv.set" => {
            let k = key()?;
            let bytes = match (p.get("value"), p.get("value_b64").and_then(Value::as_str)) {
                (_, Some(b64)) => {
                    let mut v = vec![TAG_BYTES];
                    v.extend(
                        base64::engine::general_purpose::STANDARD
                            .decode(b64)
                            .map_err(|e| invalid(format!("value_b64: {e}")))?,
                    );
                    v
                }
                (Some(val), None) => {
                    let mut v = vec![TAG_JSON];
                    v.extend(serde_json::to_vec(val).map_err(internal)?);
                    v
                }
                (None, None) => return Err(invalid("value or value_b64 is required")),
            };
            let q = quota(plugin);
            let r = server
                .with_core(|c| c.store.plugin_kv_set(plugin, k, &bytes, q))
                .map_err(internal)?;
            if let Err(e) = r {
                let (kind, reason) = match e {
                    vk_store::KvError::Key(_) => (ErrorKind::InvalidParams, "key"),
                    vk_store::KvError::ValueTooLarge(_) => {
                        (ErrorKind::InvalidParams, "value_too_large")
                    }
                    vk_store::KvError::Quota { .. } => (ErrorKind::Conflict, "quota_exceeded"),
                };
                return Err(err(kind, e.to_string()).details(json!({"quota": q, "reason": reason})));
            }
            let used = server
                .with_core(|c| c.store.plugin_kv_usage(plugin))
                .map_err(internal)?;
            Ok(json!({"key": k, "bytes": bytes.len() - 1, "used": used, "quota": q}))
        }
        "plugin.kv.delete" => {
            let k = key()?;
            let deleted = server
                .with_core(|c| c.store.plugin_kv_delete(plugin, k))
                .map_err(internal)?;
            Ok(json!({"key": k, "deleted": deleted}))
        }
        "plugin.kv.list" => {
            let prefix = p.get("prefix").and_then(Value::as_str).unwrap_or("");
            let after = p.get("after").and_then(Value::as_str);
            let limit = p
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .clamp(1, 1000) as usize;
            let rows = server
                .with_core(|c| c.store.plugin_kv_list(plugin, prefix, after, limit + 1))
                .map_err(internal)?;
            let more = rows.len() > limit;
            let keys: Vec<Value> = rows
                .iter()
                .take(limit)
                .map(|(k, n)| json!({"key": k, "bytes": n.saturating_sub(1)}))
                .collect();
            let next = more.then(|| rows[limit - 1].0.clone());
            Ok(json!({"keys": keys, "next": next}))
        }
        _ => Err(err(ErrorKind::MethodNotFound, method.to_string())),
    }
}
