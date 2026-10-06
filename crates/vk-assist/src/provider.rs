//! Provider adapters over plain HTTPS (14 §3, §10): Anthropic Messages API, OpenAI-compatible
//! chat completions and Ollama's chat API. One non-streaming request per attempt.
//!
//! Transport rules: redirects are never followed (credentials never travel cross-origin),
//! certificate validation is never disabled, bodies are bounded, provider response bodies
//! never reach error messages (they could echo content). At most one retry, only for a 429
//! before any content, honouring `Retry-After` within the deadline; every attempt (the retry
//! included) first passes the caller's gate ([`generate_gated`]), which is where the
//! coordinator re-checks consent and admits the attempt against its budget and rate window.
//! A body that fails after the headers is an error, never a shortened reply.

use crate::config::{Adapter, Resolved};
use crate::context::Payload;
use crate::{AssistError, Category};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_BODY: usize = 1 << 20;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl Usage {
    fn add(&mut self, o: Usage) {
        if let Some(i) = o.input_tokens {
            *self.input_tokens.get_or_insert(0) += i;
        }
        if let Some(i) = o.output_tokens {
            *self.output_tokens.get_or_insert(0) += i;
        }
    }
    pub fn total(&self) -> u64 {
        self.input_tokens.unwrap_or(0) + self.output_tokens.unwrap_or(0)
    }
}

#[derive(Debug)]
pub struct Outcome {
    pub result: Result<String, AssistError>,
    /// Reported usage summed over attempts (unknown stays `None`).
    pub usage: Usage,
    /// Provider attempts made (each counts toward the request limit).
    pub attempts: u32,
    pub finish_reason: Option<String>,
}

fn err(c: Category, m: impl Into<String>) -> AssistError {
    AssistError::new(c, m)
}

fn request(
    r: &Resolved,
    key: Option<&str>,
    p: &Payload,
) -> (String, Vec<(&'static str, String)>, Value) {
    let base = r.endpoint.trim_end_matches('/');
    match r.connection.adapter {
        Adapter::Anthropic => (
            format!("{base}/v1/messages"),
            [
                key.map(|k| ("x-api-key", k.to_string())),
                Some(("anthropic-version", ANTHROPIC_VERSION.to_string())),
            ]
            .into_iter()
            .flatten()
            .collect(),
            json!({
                "model": p.model,
                "max_tokens": p.max_output_tokens,
                "system": p.system,
                "messages": [{"role": "user", "content": p.user}],
            }),
        ),
        Adapter::OpenaiCompatible => (
            format!("{base}/v1/chat/completions"),
            key.map(|k| ("authorization", format!("Bearer {k}")))
                .into_iter()
                .collect(),
            json!({
                "model": p.model,
                "max_tokens": p.max_output_tokens,
                "messages": [
                    {"role": "system", "content": p.system},
                    {"role": "user", "content": p.user},
                ],
            }),
        ),
        Adapter::Ollama => (
            format!("{base}/api/chat"),
            key.map(|k| ("authorization", format!("Bearer {k}")))
                .into_iter()
                .collect(),
            json!({
                "model": p.model,
                "stream": false,
                "options": {"num_predict": p.max_output_tokens},
                "messages": [
                    {"role": "system", "content": p.system},
                    {"role": "user", "content": p.user},
                ],
            }),
        ),
    }
}

/// Parse a successful response: (text, usage, finish reason, error for refused/truncated).
fn parse(adapter: Adapter, v: &Value) -> (Option<String>, Usage, Option<String>) {
    let n = |x: &Value| x.as_u64();
    match adapter {
        Adapter::Anthropic => {
            let text: String = v["content"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            (
                v["content"].is_array().then_some(text),
                Usage {
                    input_tokens: n(&v["usage"]["input_tokens"]),
                    output_tokens: n(&v["usage"]["output_tokens"]),
                },
                v["stop_reason"].as_str().map(str::to_string),
            )
        }
        Adapter::OpenaiCompatible => (
            v["choices"][0]["message"]["content"]
                .as_str()
                .map(str::to_string),
            Usage {
                input_tokens: n(&v["usage"]["prompt_tokens"]),
                output_tokens: n(&v["usage"]["completion_tokens"]),
            },
            v["choices"][0]["finish_reason"]
                .as_str()
                .map(str::to_string),
        ),
        Adapter::Ollama => (
            v["message"]["content"].as_str().map(str::to_string),
            Usage {
                input_tokens: n(&v["prompt_eval_count"]),
                output_tokens: n(&v["eval_count"]),
            },
            v["done_reason"].as_str().map(str::to_string),
        ),
    }
}

fn finish_error(f: &str) -> Option<AssistError> {
    match f {
        "refusal" | "content_filter" => Some(err(
            Category::InvalidOutput,
            "the provider refused the request",
        )),
        "max_tokens" | "length" => Some(err(
            Category::InvalidOutput,
            "the reply was truncated at the output limit",
        )),
        _ => None,
    }
}

fn status_error(status: u16) -> AssistError {
    match status {
        401 | 403 => err(
            Category::AuthenticationFailed,
            format!("the provider rejected the credential (HTTP {status})"),
        ),
        404 => err(
            Category::NotConfigured,
            "endpoint or model not found (HTTP 404)",
        ),
        429 => err(
            Category::RateLimited,
            "the provider rate-limited the request",
        ),
        300..=399 => err(
            Category::ProviderUnavailable,
            format!("the endpoint redirected (HTTP {status}); redirects are not followed"),
        ),
        400..=499 => err(
            Category::NotConfigured,
            format!("the provider rejected the request (HTTP {status})"),
        ),
        _ => err(
            Category::ProviderUnavailable,
            format!("the provider is unavailable (HTTP {status})"),
        ),
    }
}

fn client() -> Result<reqwest::Client, AssistError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("vibeke-assist/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| err(Category::ProviderUnavailable, "HTTP client unavailable"))
}

async fn read_bounded(mut resp: reqwest::Response) -> Result<Vec<u8>, AssistError> {
    let mut body = Vec::new();
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(&chunk);
                if body.len() > MAX_BODY {
                    return Err(err(Category::InvalidOutput, "provider response too large"));
                }
            }
            Ok(None) => return Ok(body),
            // A body cut off after the headers is a failure, never a shorter reply.
            Err(e) if e.is_timeout() => {
                return Err(err(Category::Timeout, "request deadline exceeded"));
            }
            Err(_) => {
                return Err(err(
                    Category::ProviderUnavailable,
                    "the provider's response was interrupted",
                ));
            }
        }
    }
}

/// Send `p` through the resolved connection. The future can be dropped at any point
/// (cancellation aborts the connection); `deadline` bounds the whole call.
pub async fn generate(r: &Resolved, key: Option<&str>, p: &Payload, deadline: Instant) -> Outcome {
    generate_gated(r, key, p, deadline, |_| Ok(())).await
}

/// Like [`generate`], but `gate(attempt)` runs before **every** provider attempt (1-based).
/// The coordinator uses it to re-check enabled state, consent and endpoint and to admit the
/// attempt against the request budget and rate window; an `Err` stops before sending.
pub async fn generate_gated(
    r: &Resolved,
    key: Option<&str>,
    p: &Payload,
    deadline: Instant,
    mut gate: impl FnMut(u32) -> Result<(), AssistError>,
) -> Outcome {
    let mut out = Outcome {
        result: Err(err(Category::ProviderUnavailable, "not attempted")),
        usage: Usage::default(),
        attempts: 0,
        finish_reason: None,
    };
    let http = match client() {
        Ok(c) => c,
        Err(e) => {
            out.result = Err(e);
            return out;
        }
    };
    let (url, headers, body) = request(r, key, p);
    let body = body.to_string();
    loop {
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            out.result = Err(err(Category::Timeout, "request deadline exceeded"));
            return out;
        };
        if let Err(e) = gate(out.attempts + 1) {
            out.result = Err(e);
            return out;
        }
        out.attempts += 1;
        let mut rb = http
            .post(&url)
            .timeout(left)
            .header("content-type", "application/json")
            .body(body.clone());
        for (k, v) in &headers {
            rb = rb.header(*k, v);
        }
        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                out.result = Err(err(Category::Timeout, "request deadline exceeded"));
                return out;
            }
            Err(_) => {
                // Ambiguous after submission: never retried automatically.
                out.result = Err(err(
                    Category::ProviderUnavailable,
                    "could not reach the provider",
                ));
                return out;
            }
        };
        let status = resp.status().as_u16();
        if status == 429 && out.attempts == 1 {
            let wait = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            if let Some(w) = wait
                && w <= Duration::from_secs(30)
                && Instant::now() + w + Duration::from_millis(500) < deadline
            {
                tokio::time::sleep(w).await;
                continue;
            }
        }
        if !(200..300).contains(&status) {
            out.result = Err(status_error(status));
            return out;
        }
        let bytes = match read_bounded(resp).await {
            Ok(b) => b,
            Err(e) => {
                out.result = Err(e);
                return out;
            }
        };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            out.result = Err(err(
                Category::InvalidOutput,
                "provider response is not JSON",
            ));
            return out;
        };
        let (text, usage, finish) = parse(r.connection.adapter, &v);
        out.usage.add(usage);
        out.finish_reason = finish.clone();
        if let Some(e) = finish.as_deref().and_then(finish_error) {
            out.result = Err(e);
            return out;
        }
        out.result = text.ok_or_else(|| {
            err(
                Category::InvalidOutput,
                "provider response has no text content",
            )
        });
        return out;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AssistConfig;
    use crate::fake::{FakeServer, Reply};

    fn resolved(adapter: &str, endpoint: &str) -> Resolved {
        AssistConfig::from_json(json!({
            "connections": {"c": {"adapter": adapter, "endpoint": endpoint, "credential": {"env": "X"}}},
            "profiles": {"interactive": {"connection": "c", "model": "m-1"}},
        }))
        .unwrap()
        .resolve(None)
        .unwrap()
    }

    fn payload() -> Payload {
        Payload {
            adapter: "x".into(),
            model: "m-1".into(),
            max_output_tokens: 100,
            system: "sys".into(),
            user: "hello".into(),
        }
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    #[tokio::test]
    async fn anthropic_shape_and_usage() {
        let f = FakeServer::start(vec![Reply::anthropic("{\"title\":\"t\"}", 11, 7)]).await;
        let r = resolved("anthropic", &f.url());
        let o = generate(&r, Some("sk-test"), &payload(), soon()).await;
        assert_eq!(o.result.unwrap(), "{\"title\":\"t\"}");
        assert_eq!(o.usage.input_tokens, Some(11));
        assert_eq!(o.usage.output_tokens, Some(7));
        let req = &f.requests()[0];
        assert_eq!(req.path, "/v1/messages");
        assert_eq!(req.header("x-api-key").as_deref(), Some("sk-test"));
        assert_eq!(
            req.header("anthropic-version").as_deref(),
            Some(ANTHROPIC_VERSION)
        );
        assert_eq!(req.json()["model"], "m-1");
        assert_eq!(req.json()["system"], "sys");
        assert_eq!(req.json()["messages"][0]["content"], "hello");
    }

    #[tokio::test]
    async fn openai_and_ollama_shapes() {
        let f =
            FakeServer::start(vec![Reply::openai("ok", 3, 4), Reply::ollama("ok2", 5, 6)]).await;
        let o = generate(
            &resolved("openai_compatible", &f.url()),
            Some("k"),
            &payload(),
            soon(),
        )
        .await;
        assert_eq!(o.result.unwrap(), "ok");
        assert_eq!(o.usage.total(), 7);
        let o = generate(&resolved("ollama", &f.url()), None, &payload(), soon()).await;
        assert_eq!(o.result.unwrap(), "ok2");
        assert_eq!(o.usage.total(), 11);
        let reqs = f.requests();
        assert_eq!(reqs[0].path, "/v1/chat/completions");
        assert_eq!(reqs[0].header("authorization").as_deref(), Some("Bearer k"));
        assert_eq!(reqs[1].path, "/api/chat");
        assert_eq!(reqs[1].json()["stream"], false);
        assert!(reqs[1].header("authorization").is_none());
    }

    #[tokio::test]
    async fn errors_are_categorized_without_bodies() {
        let f = FakeServer::start(vec![
            Reply::status(401, "{\"error\":\"secret prompt echoed\"}"),
            Reply::status(503, "x"),
            Reply::redirect("http://127.0.0.1:1/elsewhere"),
            Reply::anthropic_stop("partial", "max_tokens"),
            Reply::anthropic_stop("", "refusal"),
        ])
        .await;
        let r = resolved("anthropic", &f.url());
        let e = generate(&r, Some("k"), &payload(), soon())
            .await
            .result
            .unwrap_err();
        assert_eq!(e.category, Category::AuthenticationFailed);
        assert!(!e.message.contains("secret"));
        let e = generate(&r, Some("k"), &payload(), soon())
            .await
            .result
            .unwrap_err();
        assert_eq!(e.category, Category::ProviderUnavailable);
        let e = generate(&r, Some("k"), &payload(), soon())
            .await
            .result
            .unwrap_err();
        assert!(e.message.contains("redirect"));
        let o = generate(&r, Some("k"), &payload(), soon()).await;
        assert_eq!(o.result.unwrap_err().category, Category::InvalidOutput);
        assert!(
            o.usage.output_tokens.is_some(),
            "usage of a truncated reply is still counted"
        );
        let e = generate(&r, Some("k"), &payload(), soon())
            .await
            .result
            .unwrap_err();
        assert!(e.message.contains("refused"));
        assert_eq!(f.requests().len(), 5, "redirect not followed");
    }

    #[tokio::test]
    async fn one_retry_after_429() {
        let f = FakeServer::start(vec![
            Reply::status(429, "").with_header("retry-after", "0"),
            Reply::anthropic("{}", 1, 1),
        ])
        .await;
        let o = generate(
            &resolved("anthropic", &f.url()),
            Some("k"),
            &payload(),
            soon(),
        )
        .await;
        assert!(o.result.is_ok());
        assert_eq!(o.attempts, 2);
        let f = FakeServer::start(vec![
            Reply::status(429, "").with_header("retry-after", "0"),
            Reply::status(429, "").with_header("retry-after", "0"),
        ])
        .await;
        let o = generate(
            &resolved("anthropic", &f.url()),
            Some("k"),
            &payload(),
            soon(),
        )
        .await;
        assert_eq!(o.result.unwrap_err().category, Category::RateLimited);
        assert_eq!(o.attempts, 2);
    }

    #[tokio::test]
    async fn every_attempt_passes_the_gate() {
        let f = FakeServer::start(vec![
            Reply::status(429, "").with_header("retry-after", "0"),
            Reply::anthropic("{}", 1, 1),
        ])
        .await;
        let mut seen = vec![];
        let o = generate_gated(
            &resolved("anthropic", &f.url()),
            Some("k"),
            &payload(),
            soon(),
            |n| {
                seen.push(n);
                if n > 1 {
                    Err(err(Category::RateLimited, "no more attempts admitted"))
                } else {
                    Ok(())
                }
            },
        )
        .await;
        assert_eq!(seen, vec![1, 2]);
        assert_eq!(o.attempts, 1);
        assert_eq!(o.result.unwrap_err().category, Category::RateLimited);
        assert_eq!(f.count(), 1, "the refused retry is never sent");
        // A gate refusing the first attempt sends nothing.
        let o = generate_gated(
            &resolved("anthropic", &f.url()),
            Some("k"),
            &payload(),
            soon(),
            |_| Err(err(Category::Disabled, "assistance was disabled")),
        )
        .await;
        assert_eq!(o.attempts, 0);
        assert_eq!(f.count(), 1);
    }

    #[tokio::test]
    async fn body_cut_after_headers_is_a_failure() {
        let f =
            FakeServer::start(vec![Reply::anthropic("{\"title\":\"t\"}", 1, 1).cut_off()]).await;
        let o = generate(
            &resolved("anthropic", &f.url()),
            Some("k"),
            &payload(),
            soon(),
        )
        .await;
        assert_eq!(
            o.result.unwrap_err().category,
            Category::ProviderUnavailable
        );
        assert_eq!(o.attempts, 1);
    }

    #[tokio::test]
    async fn deadline() {
        let f = FakeServer::start(vec![
            Reply::anthropic("{}", 1, 1).delayed(Duration::from_secs(5)),
        ])
        .await;
        let t = Instant::now();
        let o = generate(
            &resolved("anthropic", &f.url()),
            Some("k"),
            &payload(),
            Instant::now() + Duration::from_millis(300),
        )
        .await;
        assert_eq!(o.result.unwrap_err().category, Category::Timeout);
        assert!(t.elapsed() < Duration::from_secs(3));
    }
}
