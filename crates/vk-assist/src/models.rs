//! Model picker data (14 §5.2): a connection's model list, labelled **live**, **cached** or
//! **bundled** with the time it was refreshed.
//!
//! - *live*: fetched now from the provider's own listing endpoint (Anthropic `/v1/models`,
//!   OpenAI-compatible `/v1/models`, Ollama `/api/tags`, Gemini `/v1beta/models`).
//! - *cached*: a live list fetched earlier (kept by the coordinator; never refreshed silently).
//! - *bundled*: what Vibeke ships; small, possibly stale, and never a statement about what the
//!   credential can use.
//!
//! A listing says a model exists, not that this credential may use it and not what it can do:
//! existence/access and capabilities are separate. An explicit model ID is always allowed
//! whether or not it is listed.

use crate::capability::{Capabilities, Feature, Records, Support};
use crate::config::{Adapter, Resolved};
use crate::provider::{ANTHROPIC_VERSION, client, read_bounded, status_error};
use crate::{AssistError, Category, clip, sanitize};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{Duration, Instant};

const MAX_MODELS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Live,
    Cached,
    Bundled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: Option<String>,
    /// As reported by the provider (a date string or unix seconds), if at all.
    pub created: Option<String>,
    /// What this model can do, per the records Vibeke holds (not the listing alone).
    pub capabilities: Capabilities,
    /// Capability hints carried by the listing itself (applied to the records by the
    /// coordinator with source `listing`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<(Feature, Support)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelList {
    pub connection: String,
    pub adapter: String,
    pub provenance: Provenance,
    pub refreshed_at_ms: Option<i64>,
    pub models: Vec<ModelInfo>,
    pub note: String,
}

pub const ACCESS_NOTE: &str = "A listed model may not be available to this credential, and a listing does not establish capabilities. You can always enter a model id explicitly; a manually entered model starts with unknown capabilities.";

/// The shipped starting points (never a statement about access).
pub fn bundled(connection: &str, adapter: Adapter) -> ModelList {
    let ids: &[&str] = match adapter {
        Adapter::Anthropic => &[
            "claude-haiku-4-5-20251001",
            "claude-sonnet-5-5",
            "claude-opus-5-5",
        ],
        _ => &[],
    };
    ModelList {
        connection: connection.to_string(),
        adapter: adapter.as_str().to_string(),
        provenance: Provenance::Bundled,
        refreshed_at_ms: None,
        models: ids
            .iter()
            .map(|id| ModelInfo {
                id: (*id).to_string(),
                display_name: None,
                created: None,
                capabilities: Capabilities::bundled(adapter),
                hints: vec![],
            })
            .collect(),
        note: if ids.is_empty() {
            format!(
                "Vibeke bundles no models for the {} adapter; refresh to ask the provider, or enter a model id.",
                adapter.as_str()
            )
        } else {
            format!("Bundled list: it may be out of date. {ACCESS_NOTE}")
        },
    }
}

/// A list fetched earlier, labelled as such (the original refresh time is kept).
pub fn as_cached(mut l: ModelList) -> ModelList {
    l.provenance = Provenance::Cached;
    l
}

/// Fill each model's `capabilities` from `records` (bundled + observed/listed).
pub fn with_records(mut l: ModelList, records: &Records, r: &Resolved) -> ModelList {
    for m in &mut l.models {
        m.capabilities = records.recorded(r, &m.id);
    }
    l
}

fn ok_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.chars().any(|c| c.is_control())
}

/// Parse a provider's listing response into model entries (bounded, sanitized).
pub fn parse_listing(adapter: Adapter, v: &Value) -> Vec<ModelInfo> {
    let mut out = vec![];
    let mut push = |id: &str,
                    display: Option<&str>,
                    created: Option<String>,
                    hints: Vec<(Feature, Support)>| {
        let id = sanitize(id.trim());
        if !ok_id(&id) || out.len() >= MAX_MODELS {
            return;
        }
        out.push(ModelInfo {
            id,
            display_name: display.map(|d| clip(&sanitize(d), 128).0),
            created: created.map(|c| clip(&sanitize(&c), 40).0),
            capabilities: Capabilities::bundled(adapter),
            hints,
        });
    };
    match adapter {
        Adapter::Anthropic => {
            for m in v["data"].as_array().into_iter().flatten() {
                if let Some(id) = m["id"].as_str() {
                    push(
                        id,
                        m["display_name"].as_str(),
                        m["created_at"].as_str().map(str::to_string),
                        vec![],
                    );
                }
            }
        }
        Adapter::OpenaiCompatible => {
            for m in v["data"].as_array().into_iter().flatten() {
                if let Some(id) = m["id"].as_str() {
                    push(
                        id,
                        None,
                        m["created"].as_i64().map(|c| c.to_string()),
                        vec![],
                    );
                }
            }
        }
        Adapter::Ollama => {
            for m in v["models"].as_array().into_iter().flatten() {
                if let Some(id) = m["name"].as_str().or(m["model"].as_str()) {
                    push(
                        id,
                        None,
                        m["modified_at"].as_str().map(str::to_string),
                        vec![],
                    );
                }
            }
        }
        Adapter::Gemini => {
            for m in v["models"].as_array().into_iter().flatten() {
                let Some(name) = m["name"].as_str() else {
                    continue;
                };
                let methods: Vec<&str> = m["supportedGenerationMethods"]
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                // Only models that can generate text are pickable here.
                if !methods.is_empty() && !methods.contains(&"generateContent") {
                    continue;
                }
                let mut hints = vec![];
                if methods.contains(&"streamGenerateContent") {
                    hints.push((Feature::Streaming, Support::Supported));
                }
                push(
                    name.strip_prefix("models/").unwrap_or(name),
                    m["displayName"].as_str(),
                    None,
                    hints,
                );
            }
        }
    }
    out
}

fn listing_request(r: &Resolved, key: Option<&str>) -> (String, Vec<(&'static str, String)>) {
    let base = r.endpoint.trim_end_matches('/');
    match r.connection.adapter {
        Adapter::Anthropic => (
            format!("{base}/v1/models?limit=100"),
            [
                key.map(|k| ("x-api-key", k.to_string())),
                Some(("anthropic-version", ANTHROPIC_VERSION.to_string())),
            ]
            .into_iter()
            .flatten()
            .collect(),
        ),
        Adapter::OpenaiCompatible => (
            format!("{base}/v1/models"),
            key.map(|k| ("authorization", format!("Bearer {k}")))
                .into_iter()
                .collect(),
        ),
        Adapter::Ollama => (
            format!("{base}/api/tags"),
            key.map(|k| ("authorization", format!("Bearer {k}")))
                .into_iter()
                .collect(),
        ),
        Adapter::Gemini => (
            format!("{base}/v1beta/models?pageSize=200"),
            key.map(|k| ("x-goog-api-key", k.to_string()))
                .into_iter()
                .collect(),
        ),
    }
}

/// Ask the provider for its model list. The caller has already checked the connection and
/// the caller's right to refresh; this makes the one GET (no redirects, bounded body, total
/// `deadline`) and never includes the response body in an error.
pub async fn fetch(
    r: &Resolved,
    key: Option<&str>,
    deadline: Instant,
    now_ms: i64,
) -> Result<ModelList, AssistError> {
    let http = client()?;
    let (url, headers) = listing_request(r, key);
    let left = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| AssistError::new(Category::Timeout, "request deadline exceeded"))?;
    let mut rb = http.get(&url).timeout(left.min(Duration::from_secs(60)));
    for (k, v) in &headers {
        rb = rb.header(*k, v);
    }
    let resp = rb.send().await.map_err(|e| {
        if e.is_timeout() {
            AssistError::new(Category::Timeout, "request deadline exceeded")
        } else {
            AssistError::new(
                Category::ProviderUnavailable,
                "could not reach the provider",
            )
        }
    })?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(status_error(status));
    }
    let bytes = read_bounded(resp).await?;
    let v: Value = serde_json::from_slice(&bytes)
        .map_err(|_| AssistError::new(Category::InvalidOutput, "the model list is not JSON"))?;
    Ok(ModelList {
        connection: r.connection_id.clone(),
        adapter: r.connection.adapter.as_str().to_string(),
        provenance: Provenance::Live,
        refreshed_at_ms: Some(now_ms),
        models: parse_listing(r.connection.adapter, &v),
        note: ACCESS_NOTE.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AssistConfig;
    use crate::fake::{FakeServer, Reply};
    use serde_json::json;

    fn resolved(adapter: &str, endpoint: &str) -> Resolved {
        AssistConfig::from_json(json!({
            "connections": {"c": {"adapter": adapter, "endpoint": endpoint, "credential": {"env": "X"}}},
            "profiles": {"interactive": {"connection": "c", "model": "m-1"}},
        }))
        .unwrap()
        .resolve(None)
        .unwrap()
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    #[tokio::test]
    async fn live_listings_for_every_adapter() {
        let f = FakeServer::start(vec![
            Reply::json(json!({"data": [{"id": "claude-a", "display_name": "Claude A", "created_at": "2026-01-02T00:00:00Z"}], "has_more": false})),
            Reply::json(json!({"data": [{"id": "gpt-x", "created": 1700000000}, {"id": ""}]})),
            Reply::json(json!({"models": [{"name": "llama3:8b", "modified_at": "2026-02-03T00:00:00Z"}]})),
            Reply::json(json!({"models": [
                {"name": "models/gemini-pro", "displayName": "Gemini Pro", "supportedGenerationMethods": ["generateContent", "streamGenerateContent"]},
                {"name": "models/embed-1", "supportedGenerationMethods": ["embedContent"]},
            ]})),
        ])
        .await;
        let l = fetch(&resolved("anthropic", &f.url()), Some("k"), soon(), 42)
            .await
            .unwrap();
        assert_eq!(l.provenance, Provenance::Live);
        assert_eq!(l.refreshed_at_ms, Some(42));
        assert_eq!(l.models[0].id, "claude-a");
        assert_eq!(l.models[0].display_name.as_deref(), Some("Claude A"));
        let l = fetch(
            &resolved("openai_compatible", &f.url()),
            Some("k"),
            soon(),
            1,
        )
        .await
        .unwrap();
        assert_eq!(l.models.len(), 1);
        assert_eq!(l.models[0].created.as_deref(), Some("1700000000"));
        let l = fetch(&resolved("ollama", &f.url()), None, soon(), 1)
            .await
            .unwrap();
        assert_eq!(l.models[0].id, "llama3:8b");
        let l = fetch(&resolved("gemini", &f.url()), Some("gk"), soon(), 1)
            .await
            .unwrap();
        assert_eq!(l.models.len(), 1, "embedding-only models are not offered");
        assert_eq!(l.models[0].id, "gemini-pro");
        assert_eq!(
            l.models[0].hints,
            vec![(Feature::Streaming, Support::Supported)]
        );
        let reqs = f.requests();
        assert_eq!(reqs[0].method, "GET");
        assert_eq!(reqs[0].path, "/v1/models?limit=100");
        assert_eq!(reqs[0].header("x-api-key").as_deref(), Some("k"));
        assert_eq!(reqs[1].path, "/v1/models");
        assert_eq!(reqs[1].header("authorization").as_deref(), Some("Bearer k"));
        assert_eq!(reqs[2].path, "/api/tags");
        assert_eq!(reqs[3].path, "/v1beta/models?pageSize=200");
        assert_eq!(reqs[3].header("x-goog-api-key").as_deref(), Some("gk"));
    }

    #[tokio::test]
    async fn listing_errors_are_categorized_without_bodies_and_never_redirect() {
        let f = FakeServer::start(vec![
            Reply::status(401, "{\"error\":\"secret echoed\"}"),
            Reply::redirect("http://127.0.0.1:1/x"),
            Reply::status(200, "not json"),
        ])
        .await;
        let r = resolved("anthropic", &f.url());
        let e = fetch(&r, Some("k"), soon(), 0).await.unwrap_err();
        assert_eq!(e.category, Category::AuthenticationFailed);
        assert!(!e.message.contains("secret"));
        let e = fetch(&r, Some("k"), soon(), 0).await.unwrap_err();
        assert!(e.message.contains("redirect"));
        let e = fetch(&r, Some("k"), soon(), 0).await.unwrap_err();
        assert_eq!(e.category, Category::InvalidOutput);
        assert_eq!(f.count(), 3);
    }

    #[test]
    fn bundled_lists_are_labelled_and_say_nothing_about_access() {
        let l = bundled("primary", Adapter::Anthropic);
        assert_eq!(l.provenance, Provenance::Bundled);
        assert!(l.refreshed_at_ms.is_none());
        assert!(l.models.iter().any(|m| m.id.contains("haiku")));
        assert!(l.note.contains("credential"));
        let o = bundled("local", Adapter::Ollama);
        assert!(o.models.is_empty());
        assert!(o.note.contains("enter a model id"));
        assert_eq!(as_cached(l).provenance, Provenance::Cached);
    }

    #[test]
    fn listings_are_bounded_and_sanitized() {
        let many: Vec<Value> = (0..800).map(|i| json!({"id": format!("m{i}")})).collect();
        let l = parse_listing(Adapter::OpenaiCompatible, &json!({"data": many}));
        assert_eq!(l.len(), MAX_MODELS);
        let l = parse_listing(
            Adapter::OpenaiCompatible,
            &json!({"data": [{"id": "ok\u{1b}[2Jmodel"}, {"id": "x".repeat(300)}]}),
        );
        assert_eq!(l.len(), 1);
        assert!(!l[0].id.contains('\u{1b}'));
    }

    #[test]
    fn capabilities_come_from_records_not_from_the_listing_alone() {
        let r = resolved("ollama", "http://127.0.0.1:9");
        let mut rec = Records::default();
        rec.observe(&r, Feature::Streaming, Support::Supported, "finished", 0);
        let l = with_records(bundled("c", Adapter::Anthropic), &rec, &r);
        assert!(
            l.models
                .iter()
                .all(|m| !m.capabilities.usable(Feature::Streaming))
        );
        let mut live = ModelList {
            models: parse_listing(
                Adapter::Ollama,
                &json!({"models": [{"name": "m-1"}, {"name": "other"}]}),
            ),
            ..bundled("c", Adapter::Ollama)
        };
        live.provenance = Provenance::Live;
        let live = with_records(live, &rec, &r);
        assert!(live.models[0].capabilities.usable(Feature::Streaming));
        assert!(!live.models[1].capabilities.usable(Feature::Streaming));
    }
}
