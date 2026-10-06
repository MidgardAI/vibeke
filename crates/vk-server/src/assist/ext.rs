//! `assistant.models` (the model picker) and `assistant.test` (connection test and capability
//! probes), 14 §5.1, §5.2.
//!
//! - `assistant.models {connection?, profile?, refresh?}` returns a connection's model list
//!   labelled **live**, **cached** or **bundled** with its refresh time, plus each model's
//!   capability records (`supported | unsupported | unknown` with source and verification
//!   date). Without `refresh` it never touches the network. A refresh checks that assistance is
//!   enabled and that the connection resolves (endpoint rules, a resolvable credential with no
//!   ambient fallback) before sending anything, and passes the per-minute rate window.
//! - `assistant.test {profile?, probe?}` is an **explicit small generation counted as usage**:
//!   it reserves and settles budget like any request. `probe: ["streaming", "json_schema"]`
//!   additionally tries those features once each (each a counted attempt) and records what it
//!   observed, which is how a feature becomes usable on a connection. It sends no workspace
//!   content, so it needs no consent.

use super::*;
use vk_assist::capability::{Feature, Support};
use vk_assist::models as ml;
use vk_assist::provider::{self, Mode};

const TEST_SYSTEM: &str = "You are a connectivity test for Vibeke's assistant settings. Reply with one JSON object and nothing else: {\"ok\": true}";
const TEST_USER: &str = "Reply now.";

fn pick_connection(cfg: &AssistConfig, p: &Value) -> Result<Resolved, RpcError> {
    if let Some(c) = s(p, "connection") {
        return cfg.resolve_connection(c).map_err(rpc);
    }
    if let Some(pr) = s(p, "profile") {
        return cfg.resolve(Some(pr)).map_err(rpc);
    }
    if let Ok(r) = cfg.resolve(None) {
        return Ok(r);
    }
    if cfg.connections.len() == 1
        && let Some(id) = cfg.connections.keys().next()
    {
        return cfg.resolve_connection(id).map_err(rpc);
    }
    Err(ae(
        Category::NotConfigured,
        "name a connection with --connection (the default profile does not resolve)",
    ))
}

pub(super) async fn models(server: &Arc<Server>, p: &Value) -> R {
    let (cfg, _) = config()?;
    let refresh = b(p, "refresh") == Some(true);
    let resolved = pick_connection(&cfg, p)?;
    let list = if refresh {
        // Disabling assistance stops model refreshes (14 §10).
        if !cfg.enabled {
            return Err(ae(
                Category::Disabled,
                "assistance is disabled; set [assistant] enabled = true to refresh a model list",
            ));
        }
        // The connection and its credential are checked before any request leaves.
        let key = vk_assist::config::resolve_credential_with(
            &resolved.connection,
            &resolved.keychain_backend,
        )
        .map_err(rpc)?;
        lk(&state(server).rate)
            .admit(now(), cfg.requests_per_minute)
            .map_err(rpc)?;
        let deadline =
            Instant::now() + Duration::from_secs(cfg.request_timeout_seconds.clamp(1, 60));
        let live = ml::fetch(&resolved, key.as_deref(), deadline, now())
            .await
            .map_err(rpc)?;
        drop(key);
        data::store_models(server, &resolved.fingerprint, &live);
        data::apply_listing(server, &resolved, &live);
        let mut c = lk(&server.core);
        let mut tx = Tx::new();
        tx.event(
            "assistant.models_refreshed",
            json!({"assistant_connection": resolved.connection_id}),
            json!({"adapter": resolved.connection.adapter, "endpoint_host": resolved.endpoint_host(), "models": live.models.len()}),
        );
        let _ = server.commit(&mut c, tx);
        live
    } else if let Some(c) =
        data::cached_models(server, &resolved.connection_id, &resolved.fingerprint)
    {
        ml::as_cached(c)
    } else {
        ml::bundled(&resolved.connection_id, resolved.connection.adapter)
    };
    let list = ml::with_records(list, &data::records(server), &resolved);
    let current = (!resolved.profile.model.is_empty()).then(|| resolved.profile.model.clone());
    let current_caps = current.as_ref().map(|_| {
        serde_json::to_value(data::capabilities(server, &resolved)).unwrap_or(Value::Null)
    });
    let mut v = serde_json::to_value(&list).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("endpoint_host".into(), json!(resolved.endpoint_host()));
        o.insert("execution_machine".into(), json!(server.opts.machine));
        o.insert("current_model".into(), json!(current));
        o.insert("current_capabilities".into(), json!(current_caps));
        o.insert(
            "explicit_model".into(),
            json!("Any model id can be used whether or not it is listed; a manually entered model starts with unknown capabilities."),
        );
    }
    Ok(v)
}

/// One counted provider attempt outside the request lifecycle (connection test, probes).
struct Attempt {
    out: provider::Outcome,
    ms: u128,
}

async fn attempt(
    server: &Arc<Server>,
    cfg: &AssistConfig,
    id: &str,
    resolved: &Resolved,
    key: Option<&str>,
    payload: &Payload,
    mode: &Mode,
) -> Result<Attempt, RpcError> {
    let est_in = payload.estimated_input_tokens();
    // Reserve (budget, rate window) before anything is sent, exactly like a request.
    admit_attempt(server, cfg, id, resolved, est_in, payload.max_output_tokens).map_err(rpc)?;
    mark_dispatched(server, id);
    let deadline = Instant::now() + Duration::from_secs(cfg.request_timeout_seconds.max(1));
    let t = Instant::now();
    let out = provider::generate_with(
        resolved,
        key,
        payload,
        deadline,
        mode,
        |n| {
            if n == 1 {
                Ok(())
            } else {
                admit_attempt(server, cfg, id, resolved, est_in, payload.max_output_tokens)
            }
        },
        None,
    )
    .await;
    release(
        server,
        id,
        Charge::Settle {
            attempts: out.attempts,
            usage: out.usage,
            prices: resolved.prices(),
        },
    );
    Ok(Attempt {
        out,
        ms: t.elapsed().as_millis(),
    })
}

fn probe_schema() -> Value {
    json!({"type": "object", "properties": {"ok": {"type": "boolean"}}, "required": ["ok"]})
}

pub(super) async fn test(server: &Arc<Server>, p: &Value) -> R {
    let (cfg, _) = enabled_config()?;
    let resolved = cfg.resolve(s(p, "profile")).map_err(rpc)?;
    let mut probes: Vec<Feature> = vec![];
    for name in list_param(p, "probe").unwrap_or_default() {
        match Feature::parse(&name.replace('-', "_")) {
            Some(f @ (Feature::Streaming | Feature::JsonSchema)) => {
                if !probes.contains(&f) {
                    probes.push(f);
                }
            }
            _ => return Err(invalid("probe takes streaming and json_schema")),
        }
    }
    // No ambient fallback: an unresolvable reference fails before any reservation.
    let key = vk_assist::config::resolve_credential_with(
        &resolved.connection,
        &resolved.keychain_backend,
    )
    .map_err(rpc)?;
    let id = format!("test_{}", crate::core::ulid().to_lowercase());
    let payload = Payload {
        adapter: resolved.connection.adapter.as_str().into(),
        model: resolved.profile.model.clone(),
        max_output_tokens: resolved.profile.max_output_tokens.min(64),
        system: TEST_SYSTEM.into(),
        user: TEST_USER.into(),
    };
    let main = attempt(
        server,
        &cfg,
        &format!("{id}-0"),
        &resolved,
        key.as_deref(),
        &payload,
        &Mode::default(),
    )
    .await?;
    let mut usage = main.out.usage;
    let mut attempts = main.out.attempts;
    let ok = main.out.result.is_ok();
    let error = main.out.result.as_ref().err().cloned();
    let mut probe_results: Vec<Value> = vec![];
    // Probe only a connection that answered the plain test.
    for (n, f) in probes.iter().enumerate().filter(|_| ok) {
        let mode = match f {
            Feature::Streaming => Mode {
                stream: true,
                ..Mode::default()
            },
            _ => Mode {
                native_schema: Some(probe_schema()),
                ..Mode::default()
            },
        };
        let a = match attempt(
            server,
            &cfg,
            &format!("{id}-{}", n + 1),
            &resolved,
            key.as_deref(),
            &payload,
            &mode,
        )
        .await
        {
            Ok(a) => a,
            Err(e) => {
                probe_results.push(json!({"feature": f.as_str(), "ok": false, "recorded": "none", "error": {"category": e.data.details["category"], "message": e.message}}));
                continue;
            }
        };
        usage.add(a.out.usage);
        attempts += a.out.attempts;
        let worked = match (&a.out.result, f) {
            (Ok(_), Feature::Streaming) => a.out.streamed,
            (Ok(text), _) => serde_json::from_str::<Value>(text)
                .ok()
                .is_some_and(|v| v.get("ok").is_some()),
            _ => false,
        };
        let rejected = matches!(a.out.status, Some(400 | 422));
        let recorded = if worked {
            data::observe(server, &resolved, *f, Support::Supported, "probe succeeded");
            "supported"
        } else if rejected {
            data::observe(
                server,
                &resolved,
                *f,
                Support::Unsupported,
                "the provider rejected the probe request",
            );
            "unsupported"
        } else {
            "none"
        };
        probe_results.push(json!({
            "feature": f.as_str(), "ok": worked, "recorded": recorded, "latency_ms": a.ms as u64,
            "error": a.out.result.as_ref().err().map(|e| json!({"category": e.category, "message": e.message})),
        }));
    }
    drop(key);
    let cost = match (usage.input_tokens, usage.output_tokens) {
        (Some(i), Some(o)) => resolved.cost(i, o),
        _ => None,
    };
    {
        let mut c = lk(&server.core);
        let mut tx = Tx::new();
        tx.event(
            "assistant.test_finished",
            json!({"assistant_connection": resolved.connection_id}),
            json!({
                "ok": ok, "adapter": resolved.connection.adapter, "model": resolved.profile.model,
                "endpoint_host": resolved.endpoint_host(), "attempts": attempts,
                "input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens,
                "error_category": error.as_ref().map(|e| e.category),
            }),
        );
        let _ = server.commit(&mut c, tx);
    }
    Ok(json!({
        "ok": ok,
        "id": id,
        "profile": resolved.profile_id,
        "connection": resolved.connection_id,
        "adapter": resolved.connection.adapter,
        "model": resolved.profile.model,
        "endpoint_host": resolved.endpoint_host(),
        "execution_machine": server.opts.machine,
        "latency_ms": main.ms as u64,
        "attempts": attempts,
        "usage": usage,
        "estimated_cost_usd": cost,
        "counted": true,
        "error": error.map(|e| json!({"category": e.category, "message": e.message})),
        "probes": probe_results,
    }))
}
