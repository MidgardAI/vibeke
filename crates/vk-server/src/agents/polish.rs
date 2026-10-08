//! Adapter polish API (04 §7.7, §10, §12.3, §13): `agent.turn_usage`,
//! `agent.limits`, `agent.drift`, `agent.manifests_check` and `agent.manifest_pin`. One hook in
//! `agents::api`; everything else lives in the modules these call into.

use super::*;

pub(super) async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "agent.turn_usage" => turn_usage(server, ctx, p),
        "agent.limits" => Ok(json!({"limits": usage::limits(server)})),
        "agent.drift" => Ok(json!({"versions": arbiter::snapshot()})),
        "agent.manifests_check" => manifests_check(server, p).await,
        "agent.manifest_pin" => manifest_pin(p),
        _ => return None,
    })
}

// ---- usage ------------------------------------------------------------------------------------

fn turn_usage(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let run = resolve_run(server, ctx, s(p, "run").or(s(p, "target")))?;
    let limit = p
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(200)
        .min(2000) as usize;
    let mut turns = tailer::records_of(server, &run.id);
    let total = turns.len();
    if turns.len() > limit {
        turns.drain(..turns.len() - limit);
    }
    let sum = |f: &dyn Fn(&tailer::TurnUsageRecord) -> u64| turns.iter().map(f).sum::<u64>();
    let cost: f64 = turns.iter().filter_map(|t| t.cost_usd).sum();
    Ok(json!({
        "run": run.id,
        "turns": turns,
        "turn_count": total,
        "totals": {
            "input": sum(&|t| t.input), "output": sum(&|t| t.output),
            "cache_read": sum(&|t| t.cache_read), "cache_write": sum(&|t| t.cache_write),
            "cost_usd": turns.iter().any(|t| t.cost_usd.is_some()).then_some(cost),
        },
        "usage": run.usage,
    }))
}

// ---- manifest channel -------------------------------------------------------------------------

async fn manifests_check(server: &Arc<Server>, p: &Value) -> R {
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    let url = s(p, "url")
        .map(str::to_string)
        .unwrap_or_else(|| channel::channel_url(&cfg));
    let srv = server.clone();
    let res = tokio::task::spawn_blocking(move || {
        match channel::update(&url, &channel::root()) {
            Ok(r) => {
                let announced = channel::announce_loaded(&srv);
                let _ = manifests::reload();
                Ok(json!({"serial": r.serial, "applied": r.applied, "skipped": r.skipped, "unsigned": r.unsigned, "warnings": r.warnings, "announced": announced}))
            }
            Err(channel::ChannelError::Rollback { have, got }) => {
                Ok(json!({"serial": have, "applied": [], "unchanged": true, "index_serial": got}))
            }
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .map_err(|e| internal(e.to_string()))?;
    res.map_err(|e| err(ErrorKind::RemoteUnavailable, e))
}

fn manifest_pin(p: &Value) -> R {
    let id = crate::api::req(p, "id")?;
    let root = channel::root();
    if p.get("unpin").and_then(Value::as_bool).unwrap_or(false) {
        let had = channel::unpin(&root, id).map_err(|e| invalid(e.to_string()))?;
        return Ok(json!({"id": id, "unpinned": had}));
    }
    let pin = channel::pin(&root, id, s(p, "version")).map_err(|e| invalid(e.to_string()))?;
    Ok(json!({"id": id, "version": pin.version, "pinned_at_ms": pin.pinned_at_ms}))
}
