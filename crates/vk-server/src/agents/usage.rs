//! Usage and rate-limit extraction (04 §10) into `AgentRun.usage` / `AgentRun.rate_limit`.
//!
//! Sources:
//! - Claude: transcript JSONL (`transcript_path` from hooks) at every `Stop` — assistant
//!   `message.usage {input_tokens, output_tokens, cache_read_input_tokens,
//!   cache_creation_input_tokens}`, de-duplicated by `message.id` (Claude writes one line per
//!   content block with the same usage); `StopFailure{rate_limit}` and
//!   `Notification{quota_auto_resume_*}` → rate limited.
//! - Codex: rollout JSONL at `Stop` — the last `event_msg` `token_count` (`info.total_token_usage`,
//!   `rate_limits.primary/secondary {used_percent, window_minutes, resets_at|resets_in_seconds}`).
//! - pi/omp: the extension's `Usage` signal (per message; summed).
//! - OpenCode: completed assistant `message.updated` (`tokens`, `cost`) [verify M2].
//! - Gemini: `AfterModel` `llm_response.usageMetadata` when that hook is installed [verify M2].
//! - ACP: `usage` on the `session/prompt` result where an agent reports it (not in the base
//!   protocol; unverified per agent).

use super::*;
use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;
use vk_agents::pricing::PriceTable;
use vk_agents::transcript::TurnUsage;

const MAX_TRANSCRIPT: u64 = 64 << 20;

pub fn looks_rate_limited(msg: &str) -> bool {
    let m = msg.to_lowercase();
    [
        "rate limit",
        "rate_limit",
        "ratelimit",
        "429",
        "quota",
        "too many requests",
        "usage limit",
    ]
    .iter()
    .any(|k| m.contains(k))
}

/// Mark the run rate-limited (execution `rate_limited`, 04 §2.4).
pub fn rate_limited(server: &Server, run: &AgentRun, resets_at_ms: Option<i64>, msg: Option<&str>) {
    update_run(server, &run.id, |r, tx| {
        r.rate_limit = Some(RateLimitInfo {
            limited: true,
            resets_at_ms,
            scope: None,
            used_percent: None,
            message: msg.map(str::to_string),
            observed_at_ms: now_ms(),
        });
        tx.event(
            "agent.rate_limited",
            json!({"run": r.id, "pane": r.pane}),
            json!({"resets_at_ms": resets_at_ms, "message": msg}),
        );
    });
    set_execution(
        server,
        &run.id,
        Execution::RateLimited,
        StateSource::Structured,
        1.0,
        msg.map(str::to_string),
    );
}

fn store(server: &Server, run: &str, usage: Option<RunUsage>, limit: Option<RateLimitInfo>) {
    if usage.is_none() && limit.is_none() {
        return;
    }
    let stream_usage = usage.clone();
    update_run(server, run, |r, tx| {
        // No harness-reported cost: estimate it from the price table (04 §10), unless the run is
        // subscription-billed.
        let usage = usage.map(|mut u| {
            if u.cost_usd.is_none() {
                let tu = TurnUsage {
                    input: u.input_tokens,
                    output: u.output_tokens,
                    cache_read: u.cache_read_tokens,
                    cache_write: u.cache_write_tokens,
                    cost_usd: None,
                };
                u.cost_usd = cost_for(
                    server,
                    billing_of(&r.harness).as_deref(),
                    u.model.as_deref(),
                    &tu,
                )
                .0;
            }
            u
        });
        if let Some(u) = usage
            && u != r.usage
        {
            tx.event(
                "agent.usage",
                json!({"run": r.id, "pane": r.pane}),
                json!({"input": u.input_tokens, "output": u.output_tokens, "cache_read": u.cache_read_tokens, "cache_write": u.cache_write_tokens, "cost_usd": u.cost_usd, "source": u.source}),
            );
            r.usage = u;
        }
        if let Some(l) = limit {
            r.rate_limit = Some(l);
        }
    });
    // Per-turn usage of the Turn/Item stream (02 §1.1): after the run update, never inside it
    // (the stream takes the core lock itself).
    if let Some(u) = &stream_usage {
        crate::items::usage_updated(server, run, u);
    }
}

/// Hook-vocabulary events (`on_signal`).
pub(super) fn observe(server: &Arc<Server>, run: &AgentRun, event: &str, p: &Value) {
    match event {
        "Stop" => {
            // A finished turn clears a held limit (the observation is kept for display).
            if run.rate_limit.as_ref().is_some_and(|l| l.limited) {
                update_run(server, &run.id, |r, _| {
                    if let Some(l) = r.rate_limit.as_mut() {
                        l.limited = false;
                    }
                });
            }
            let path = p
                .get("transcript_path")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| run.transcript_path.clone());
            let Some(path) = path else { return };
            let srv = server.clone();
            let id = run.id.clone();
            tokio::spawn(async move {
                let parsed =
                    tokio::task::spawn_blocking(move || transcript_usage(Path::new(&path)))
                        .await
                        .ok()
                        .flatten();
                if let Some((u, l)) = parsed {
                    store(&srv, &id, u, l);
                }
            });
        }
        "StopFailure" => {
            let kind = p
                .get("error_type")
                .or_else(|| p.get("matcher"))
                .or_else(|| p.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if kind.contains("rate") {
                let msg = p.get("message").and_then(Value::as_str).unwrap_or(kind);
                update_run(server, &run.id, |r, _| {
                    r.rate_limit = Some(RateLimitInfo {
                        limited: true,
                        resets_at_ms: None,
                        scope: None,
                        used_percent: None,
                        message: Some(msg.to_string()),
                        observed_at_ms: now_ms(),
                    });
                });
            }
        }
        "Notification" => {
            let ty = p
                .get("notification_type")
                .or_else(|| p.get("matcher"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if ty.starts_with("quota_auto_resume") {
                let msg = p.get("message").and_then(Value::as_str).map(str::to_string);
                update_run(server, &run.id, |r, _| {
                    r.rate_limit = Some(RateLimitInfo {
                        limited: true,
                        resets_at_ms: None,
                        scope: Some(ty.to_string()),
                        used_percent: None,
                        message: msg,
                        observed_at_ms: now_ms(),
                    });
                });
            }
        }
        _ => {}
    }
}

/// Tests set billing per harness id here instead of writing the shared config file.
#[cfg(test)]
pub(crate) static BILLING_OVERRIDE: LazyLock<Mutex<std::collections::HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// `[agents.harness.<id>] billing` (`subscription` | `api`).
pub(super) fn billing_of(harness: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(b) = BILLING_OVERRIDE.lock().unwrap().get(harness) {
        return Some(b.clone());
    }
    vk_config::Config::load(vk_config::config_path())
        .ok()
        .and_then(|(c, _)| {
            c.agents
                .harness
                .get(harness)
                .and_then(|h| h.billing.clone())
        })
}

type PriceCache = Mutex<Option<(i64, PriceTable)>>;
static PRICES: LazyLock<PriceCache> = LazyLock::new(|| Mutex::new(None));

/// The price table in force: a signed `<state>/prices/prices.json` with a higher serial than the
/// bundled one (verified like manifest channel indexes: embedded release keys; unsigned only
/// with `VIBEKE_ALLOW_UNSIGNED_MANIFESTS=1`), else the bundled table.
pub(super) fn price_table(server: &Server) -> PriceTable {
    let file = server.paths.state.join("prices/prices.json");
    let mtime = std::fs::metadata(&file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    {
        let g = PRICES.lock().unwrap();
        if let Some((t, tab)) = g.as_ref()
            && *t == mtime
        {
            return tab.clone();
        }
    }
    let chosen = load_signed_prices(&file, mtime)
        .filter(|t| t.serial > PriceTable::bundled().serial)
        .unwrap_or_else(|| PriceTable::bundled().clone());
    *PRICES.lock().unwrap() = Some((mtime, chosen.clone()));
    chosen
}

fn load_signed_prices(file: &Path, mtime: i64) -> Option<PriceTable> {
    if mtime == 0 {
        return None;
    }
    let bytes = std::fs::read(file).ok()?;
    let sig = std::fs::read(file.with_extension("json.minisig")).ok();
    match super::channel::verify_signature(&bytes, sig.as_deref()) {
        Ok(()) => {}
        Err(e) if super::channel::allow_unsigned_env() => {
            tracing::warn!("price table: using an UNSIGNED table ({e})");
        }
        Err(e) => {
            tracing::warn!("price table ignored: {e}");
            return None;
        }
    }
    PriceTable::parse(&String::from_utf8(bytes).ok()?).ok()
}

/// Cost of a token mix and where it came from (04 §10): the harness's own figure, else the price
/// table by model id; subscription-billed runs keep tokens only.
pub(super) fn cost_for(
    server: &Server,
    billing: Option<&str>,
    model: Option<&str>,
    u: &TurnUsage,
) -> (Option<f64>, &'static str) {
    // The table is only read when nothing else decides the cost.
    if billing == Some("subscription") || u.cost_usd.is_some() || model.is_none() {
        return cost_with(PriceTable::bundled(), billing, model, u);
    }
    cost_with(&price_table(server), billing, model, u)
}

/// [`cost_for`] against an explicit table.
pub fn cost_with(
    table: &PriceTable,
    billing: Option<&str>,
    model: Option<&str>,
    u: &TurnUsage,
) -> (Option<f64>, &'static str) {
    if billing == Some("subscription") {
        return (None, "subscription");
    }
    if let Some(c) = u.cost_usd {
        return (Some(c), "harness");
    }
    let Some(m) = model else {
        return (None, "none");
    };
    match table.cost(m, u.input, u.output, u.cache_read, u.cache_write) {
        Some(c) => (Some(c), "price_table"),
        None => (None, "none"),
    }
}

/// The TranscriptTailer's running totals (every completed turn so far) become the run's usage.
pub(super) fn from_tailer(
    server: &Server,
    run: &AgentRun,
    t: &TurnUsage,
    model: Option<&str>,
    billing: Option<&str>,
) {
    let same_tokens = run.usage.input_tokens == t.input
        && run.usage.output_tokens == t.output
        && run.usage.cache_read_tokens == t.cache_read
        && run.usage.cache_write_tokens == t.cache_write;
    let (cost, _) = cost_for(server, billing, model.or(run.usage.model.as_deref()), t);
    if same_tokens && (cost.is_none() || run.usage.cost_usd.is_some()) {
        return;
    }
    let d = RunUsage {
        input_tokens: t.input,
        output_tokens: t.output,
        cache_read_tokens: t.cache_read,
        cache_write_tokens: t.cache_write,
        cost_usd: cost,
        model: model.map(str::to_string).or(run.usage.model.clone()),
        source: "transcript".into(),
        updated_at_ms: now_ms(),
    };
    store(server, &run.id, Some(d), None);
}

/// Per-harness limits status (04 §10 "limits" segment): the most recent observation per harness
/// from live runs, most constrained window first.
pub fn limits(server: &Server) -> Vec<Value> {
    let runs: Vec<AgentRun> = server.with_core(|c| {
        c.model
            .runs
            .iter()
            .filter(|r| r.ended_at_ms.is_none() && r.rate_limit.is_some())
            .cloned()
            .collect()
    });
    let mut by: std::collections::BTreeMap<String, RateLimitInfo> = Default::default();
    for r in runs {
        let Some(l) = r.rate_limit else { continue };
        match by.get(&r.harness) {
            Some(cur) if cur.observed_at_ms >= l.observed_at_ms => {}
            _ => {
                by.insert(r.harness, l);
            }
        }
    }
    let mut v: Vec<Value> = by
        .into_iter()
        .map(|(h, l)| {
            json!({
                "harness": h, "scope": l.scope, "limited": l.limited,
                "used_percent": l.used_percent, "resets_at_ms": l.resets_at_ms,
                "message": l.message, "observed_at_ms": l.observed_at_ms,
            })
        })
        .collect();
    v.sort_by(|a, b| {
        let p = |x: &Value| x["used_percent"].as_f64().unwrap_or(0.0);
        p(b).total_cmp(&p(a))
    });
    v
}

fn u64_of(v: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|k| v.pointer(k).and_then(Value::as_u64))
        .unwrap_or(0)
}

fn add(run: &AgentRun, delta: RunUsage) -> RunUsage {
    let mut u = run.usage.clone();
    u.input_tokens += delta.input_tokens;
    u.output_tokens += delta.output_tokens;
    u.cache_read_tokens += delta.cache_read_tokens;
    u.cache_write_tokens += delta.cache_write_tokens;
    u.cost_usd = match (u.cost_usd, delta.cost_usd) {
        (Some(a), Some(b)) => Some(a + b),
        (a, b) => a.or(b),
    };
    u.model = delta.model.or(u.model);
    u.source = delta.source;
    u.updated_at_ms = now_ms();
    u
}

/// pi/omp extension `Usage {input, output, cache_read, cache_write, cost?}` (per message).
pub(super) fn from_extension(server: &Server, run: &AgentRun, p: &Value) {
    let d = RunUsage {
        input_tokens: u64_of(p, &["/input"]),
        output_tokens: u64_of(p, &["/output"]),
        cache_read_tokens: u64_of(p, &["/cache_read", "/cacheRead"]),
        cache_write_tokens: u64_of(p, &["/cache_write", "/cacheWrite"]),
        cost_usd: p.get("cost").and_then(|c| {
            c.as_f64()
                .or_else(|| c.get("total").and_then(Value::as_f64))
        }),
        model: p.get("model").and_then(Value::as_str).map(str::to_string),
        source: "extension".into(),
        updated_at_ms: 0,
    };
    store(server, &run.id, Some(add(run, d)), None);
}

static SEEN: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// OpenCode assistant `message.updated` info: `tokens {input, output, reasoning, cache {read,
/// write}}`, `cost`, `modelID` — counted once per message id.
pub(super) fn from_opencode(server: &Server, run: &AgentRun, info: &Value) {
    if info.get("tokens").is_none() {
        return;
    }
    if let Some(id) = info.get("id").and_then(Value::as_str)
        && !SEEN.lock().unwrap().insert(format!("{}:{id}", run.id))
    {
        return;
    }
    let d = RunUsage {
        input_tokens: u64_of(info, &["/tokens/input"]),
        output_tokens: u64_of(info, &["/tokens/output"]) + u64_of(info, &["/tokens/reasoning"]),
        cache_read_tokens: u64_of(info, &["/tokens/cache/read"]),
        cache_write_tokens: u64_of(info, &["/tokens/cache/write"]),
        cost_usd: info.get("cost").and_then(Value::as_f64),
        model: info
            .get("modelID")
            .and_then(Value::as_str)
            .map(str::to_string),
        source: "extension".into(),
        updated_at_ms: 0,
    };
    store(server, &run.id, Some(add(run, d)), None);
}

/// Gemini `AfterModel` payload: `llm_response.usageMetadata`.
pub(super) fn from_gemini(server: &Server, run: &AgentRun, p: &Value) {
    let m = p
        .pointer("/llm_response/usageMetadata")
        .or_else(|| p.get("usageMetadata"));
    let Some(m) = m else { return };
    let d = RunUsage {
        input_tokens: u64_of(m, &["/promptTokenCount"]),
        output_tokens: u64_of(m, &["/candidatesTokenCount"]) + u64_of(m, &["/thoughtsTokenCount"]),
        cache_read_tokens: u64_of(m, &["/cachedContentTokenCount"]),
        cache_write_tokens: 0,
        cost_usd: None,
        model: p
            .pointer("/llm_request/model")
            .and_then(Value::as_str)
            .map(str::to_string),
        source: "hook".into(),
        updated_at_ms: 0,
    };
    store(server, &run.id, Some(add(run, d)), None);
}

/// Headless adapters (04 §10): a per-turn delta (Claude `result.usage`, pi `turn_end`, ACP) or
/// the session total (Codex `thread/tokenUsage/updated`).
pub(super) fn from_headless(server: &Server, run: &AgentRun, u: RunUsage, total: bool) {
    let next = if total {
        RunUsage {
            cost_usd: u.cost_usd.or(run.usage.cost_usd),
            model: u.model.clone().or(run.usage.model.clone()),
            updated_at_ms: now_ms(),
            ..u
        }
    } else {
        add(run, u)
    };
    store(server, &run.id, Some(next), None);
}

/// A headless rate-limit snapshot (Codex `account/rateLimits/updated`, 04 §6.2, §10): stored on
/// the run; a window at 100 % marks the run rate-limited (once, until it is cleared).
pub(super) fn from_headless_limit(server: &Server, run: &AgentRun, l: RateLimitInfo) {
    let was = run.rate_limit.as_ref().is_some_and(|x| x.limited);
    if l.limited && !was {
        let msg = format!(
            "{} rate-limit window at {:.0}%",
            l.scope.as_deref().unwrap_or("usage"),
            l.used_percent.unwrap_or(100.0)
        );
        rate_limited(server, run, l.resets_at_ms, Some(&msg));
    }
    store(server, &run.id, None, Some(l));
}

/// Codex app-server rate limits (`{rateLimits: {primary, secondary}}`, each `{usedPercent,
/// windowDurationMins, resetsAt}` with `resetsAt` in epoch seconds; snake_case accepted): the
/// most constrained window.
pub fn app_server_rate_limit(p: &Value, now: i64) -> Option<RateLimitInfo> {
    let rl = p
        .get("rateLimits")
        .or_else(|| p.get("rate_limits"))
        .unwrap_or(p);
    let mut best: Option<RateLimitInfo> = None;
    for scope in ["primary", "secondary"] {
        let Some(w) = rl.get(scope).filter(|w| w.is_object()) else {
            continue;
        };
        let used = w
            .get("usedPercent")
            .or_else(|| w.get("used_percent"))
            .and_then(Value::as_f64)
            .map(|f| f as f32);
        let resets = w
            .get("resetsAt")
            .or_else(|| w.get("resets_at"))
            .and_then(Value::as_i64)
            .map(|s| s * 1000)
            .or_else(|| {
                w.get("resetsInSeconds")
                    .or_else(|| w.get("resets_in_seconds"))
                    .and_then(Value::as_i64)
                    .map(|s| now + s * 1000)
            });
        let info = RateLimitInfo {
            limited: used.is_some_and(|u| u >= 100.0),
            resets_at_ms: resets,
            scope: Some(scope.to_string()),
            used_percent: used,
            message: None,
            observed_at_ms: now,
        };
        if best
            .as_ref()
            .is_none_or(|b| b.used_percent.unwrap_or(0.0) < info.used_percent.unwrap_or(0.0))
        {
            best = Some(info);
        }
    }
    best
}

/// ACP `usage` on a prompt result (camelCase or snake_case token fields).
pub(super) fn from_acp(server: &Server, run: &AgentRun, u: &Value) {
    let d = RunUsage {
        input_tokens: u64_of(u, &["/inputTokens", "/input_tokens"]),
        output_tokens: u64_of(u, &["/outputTokens", "/output_tokens"]),
        cache_read_tokens: u64_of(
            u,
            &[
                "/cachedReadTokens",
                "/cacheReadTokens",
                "/cache_read_tokens",
            ],
        ),
        cache_write_tokens: u64_of(
            u,
            &[
                "/cachedWriteTokens",
                "/cacheWriteTokens",
                "/cache_write_tokens",
            ],
        ),
        cost_usd: u
            .get("costUsd")
            .or_else(|| u.get("cost_usd"))
            .and_then(Value::as_f64),
        model: None,
        source: "acp".into(),
        updated_at_ms: 0,
    };
    store(server, &run.id, Some(add(run, d)), None);
}

fn read_capped(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(MAX_TRANSCRIPT);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    if start > 0
        && let Some(i) = s.find('\n')
    {
        s.drain(..=i);
    }
    Some(s)
}

/// Usage (and Codex rate limits) from a Claude or Codex transcript.
pub fn transcript_usage(path: &Path) -> Option<(Option<RunUsage>, Option<RateLimitInfo>)> {
    let text = read_capped(path)?;
    if text.contains("\"event_msg\"") || text.contains("\"response_item\"") {
        Some(codex_usage(&text))
    } else {
        Some((claude_usage(&text), None))
    }
}

pub fn claude_usage(text: &str) -> Option<RunUsage> {
    // message id → usage (the last line per message wins).
    let mut by_msg: Vec<(String, Value)> = vec![];
    let mut model = None;
    let mut cost: Option<f64> = None;
    for (n, l) in text.lines().enumerate() {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        if let Some(c) = v.get("costUSD").and_then(Value::as_f64) {
            *cost.get_or_insert(0.0) += c;
        }
        let Some(u) = v.pointer("/message/usage") else {
            continue;
        };
        if let Some(m) = v.pointer("/message/model").and_then(Value::as_str) {
            model = Some(m.to_string());
        }
        let id = v
            .pointer("/message/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("line{n}"));
        match by_msg.iter_mut().find(|(i, _)| *i == id) {
            Some(e) => e.1 = u.clone(),
            None => by_msg.push((id, u.clone())),
        }
    }
    if by_msg.is_empty() {
        return None;
    }
    let mut out = RunUsage {
        model,
        cost_usd: cost,
        source: "transcript".into(),
        updated_at_ms: now_ms(),
        ..Default::default()
    };
    for (_, u) in by_msg {
        out.input_tokens += u64_of(&u, &["/input_tokens"]);
        out.output_tokens += u64_of(&u, &["/output_tokens"]);
        out.cache_read_tokens += u64_of(&u, &["/cache_read_input_tokens"]);
        out.cache_write_tokens += u64_of(&u, &["/cache_creation_input_tokens"]);
    }
    Some(out)
}

pub fn codex_usage(text: &str) -> (Option<RunUsage>, Option<RateLimitInfo>) {
    let mut usage = None;
    let mut limit = None;
    let mut model = None;
    for l in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
        if ty == "turn_context"
            && let Some(m) = v.pointer("/payload/model").and_then(Value::as_str)
        {
            model = Some(m.to_string());
        }
        if ty != "event_msg"
            || v.pointer("/payload/type").and_then(Value::as_str) != Some("token_count")
        {
            continue;
        }
        let p = &v["payload"];
        if let Some(t) = p.pointer("/info/total_token_usage") {
            let cached = u64_of(t, &["/cached_input_tokens"]);
            usage = Some(RunUsage {
                // Codex's input count includes the cached part.
                input_tokens: u64_of(t, &["/input_tokens"]).saturating_sub(cached),
                output_tokens: u64_of(t, &["/output_tokens"])
                    + u64_of(t, &["/reasoning_output_tokens"]),
                cache_read_tokens: cached,
                cache_write_tokens: 0,
                cost_usd: None,
                model: model.clone(),
                source: "transcript".into(),
                updated_at_ms: now_ms(),
            });
        }
        let ts_ms = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_ms)
            .unwrap_or_else(now_ms);
        // The most constrained window (highest used_percent) is the one worth showing.
        let mut best: Option<RateLimitInfo> = None;
        for scope in ["primary", "secondary"] {
            let Some(w) = p
                .pointer(&format!("/rate_limits/{scope}"))
                .filter(|w| w.is_object())
            else {
                continue;
            };
            let used = w
                .get("used_percent")
                .and_then(Value::as_f64)
                .map(|f| f as f32);
            let resets = w
                .get("resets_at")
                .and_then(Value::as_i64)
                .map(|s| s * 1000)
                .or_else(|| {
                    w.get("resets_in_seconds")
                        .and_then(Value::as_i64)
                        .map(|s| ts_ms + s * 1000)
                });
            let info = RateLimitInfo {
                limited: used.is_some_and(|u| u >= 100.0),
                resets_at_ms: resets,
                scope: Some(scope.to_string()),
                used_percent: used,
                message: None,
                observed_at_ms: ts_ms,
            };
            if best
                .as_ref()
                .is_none_or(|b| b.used_percent.unwrap_or(0.0) < info.used_percent.unwrap_or(0.0))
            {
                best = Some(info);
            }
        }
        if best.is_some() {
            limit = best;
        }
    }
    (usage, limit)
}

/// `2026-10-06T12:00:00.123Z` → epoch ms (UTC only; enough for rollout timestamps).
pub(crate) fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, mo, da) = (d.next()??, d.next()??, d.next()??);
    let time = time.trim_end_matches('Z');
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':').map(|x| x.parse::<i64>().ok());
    let (h, mi, se) = (t.next()??, t.next()??, t.next()??);
    let ms: i64 = format!("{:0<3}", &frac[..frac.len().min(3)]).parse().ok()?;
    // Days from civil (Howard Hinnant).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + da - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 86400 + h * 3600 + mi * 60 + se) * 1000) + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_transcript_usage_dedupes_message_ids() {
        let t = [
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"id":"msg_1","model":"claude-opus-4","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":20}}}"#,
            r#"{"type":"assistant","message":{"id":"msg_1","model":"claude-opus-4","usage":{"input_tokens":10,"output_tokens":7,"cache_read_input_tokens":100,"cache_creation_input_tokens":20}}}"#,
            r#"{"type":"assistant","message":{"id":"msg_2","model":"claude-opus-4","usage":{"input_tokens":3,"output_tokens":2}}}"#,
            "not json",
        ]
        .join("\n");
        let u = claude_usage(&t).unwrap();
        assert_eq!(
            (
                u.input_tokens,
                u.output_tokens,
                u.cache_read_tokens,
                u.cache_write_tokens
            ),
            (13, 9, 100, 20)
        );
        assert_eq!(u.model.as_deref(), Some("claude-opus-4"));
        assert_eq!(u.source, "transcript");
        assert!(claude_usage("{\"type\":\"user\"}").is_none());
    }

    #[test]
    fn codex_rollout_usage_and_limits() {
        let t = [
            r#"{"timestamp":"2026-10-06T12:00:00.000Z","type":"turn_context","payload":{"model":"gpt-5-codex"}}"#,
            r#"{"timestamp":"2026-10-06T12:00:01.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":600,"output_tokens":50,"reasoning_output_tokens":25}},"rate_limits":{"primary":{"used_percent":42.0,"window_minutes":300,"resets_in_seconds":3600},"secondary":{"used_percent":100.0,"window_minutes":10080,"resets_at":1791374400}}}}"#,
        ]
        .join("\n");
        let (u, l) = codex_usage(&t);
        let u = u.unwrap();
        assert_eq!(
            (u.input_tokens, u.cache_read_tokens, u.output_tokens),
            (400, 600, 75)
        );
        assert_eq!(u.model.as_deref(), Some("gpt-5-codex"));
        let l = l.unwrap();
        assert_eq!(l.scope.as_deref(), Some("secondary"));
        assert!(l.limited);
        assert_eq!(l.resets_at_ms, Some(1_791_374_400_000));
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("rollout.jsonl");
        std::fs::write(&p, &t).unwrap();
        assert!(transcript_usage(&p).unwrap().1.is_some());
        assert_eq!(parse_rfc3339_ms("1970-01-02T00:00:01.5Z"), Some(86_401_500));
    }

    #[test]
    fn cost_prefers_the_harness_figure_then_the_table_and_hides_subscriptions() {
        let table = PriceTable::parse(
            r#"{"serial":1,"prices":[{"model":"claude-sonnet-4","input":3.0,"output":15.0,"cache_read":0.3,"cache_write":3.75}]}"#,
        )
        .unwrap();
        let mut u = TurnUsage {
            input: 1_000_000,
            output: 100_000,
            ..Default::default()
        };
        let (c, src) = cost_with(&table, None, Some("claude-sonnet-4-20250514"), &u);
        assert_eq!(src, "price_table");
        assert!((c.unwrap() - 4.5).abs() < 1e-9);
        assert_eq!(
            cost_with(&table, Some("subscription"), Some("claude-sonnet-4"), &u),
            (None, "subscription")
        );
        assert_eq!(
            cost_with(&table, Some("api"), Some("unknown-model"), &u),
            (None, "none")
        );
        assert_eq!(cost_with(&table, None, None, &u), (None, "none"));
        u.cost_usd = Some(0.5);
        assert_eq!(
            cost_with(&table, None, Some("claude-sonnet-4"), &u),
            (Some(0.5), "harness")
        );
        // Subscription wins even over a harness figure: tokens only.
        assert_eq!(
            cost_with(&table, Some("subscription"), Some("claude-sonnet-4"), &u),
            (None, "subscription")
        );
    }

    #[test]
    fn rate_limit_messages() {
        assert!(looks_rate_limited("Error 429: Too Many Requests"));
        assert!(looks_rate_limited("You've hit your usage limit"));
        assert!(!looks_rate_limited("connection reset"));
    }
}
