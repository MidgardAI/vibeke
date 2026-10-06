//! Provider adapters over plain HTTPS (14 §3, §10): Anthropic Messages API, OpenAI-compatible
//! chat completions, Ollama's chat API and Google's native Gemini `generateContent`.
//!
//! Transport rules: redirects are never followed (credentials never travel cross-origin),
//! certificate validation is never disabled, bodies are bounded, provider response bodies
//! never reach error messages (they could echo content). At most one retry, only for a 429
//! before any content, honouring `Retry-After` within the deadline; every attempt (the retry
//! included) first passes the caller's gate ([`generate_gated`]), which is where the
//! coordinator re-checks consent and admits the attempt against its budget and rate window.
//! A body that fails after the headers is an error, never a shortened reply.
//!
//! [`Mode`] selects the optional wire features: **native structured output** (a JSON schema in
//! the provider's own mechanism) and **streaming** (text deltas reported to a sink as they
//! arrive). Neither is chosen here: the caller passes them only for capabilities recorded as
//! `supported` ([`crate::capability`]). A stream that ends without its terminal event is a
//! failure, and a partially delivered stream is never retried.

use crate::config::{Adapter, Resolved};
use crate::context::Payload;
use crate::{AssistError, Category};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const MAX_BODY: usize = 1 << 20;
/// Name of the forced tool that carries native structured output on Anthropic.
pub const TOOL_NAME: &str = "emit_result";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

impl Usage {
    pub fn add(&mut self, o: Usage) {
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

/// Optional wire features for one call.
#[derive(Debug, Clone, Default)]
pub struct Mode {
    /// Ask for native structured output with this JSON schema (only for a `supported`
    /// `json_schema` capability).
    pub native_schema: Option<Value>,
    /// Stream text deltas to the sink (only for a `supported` `streaming` capability, or when
    /// the caller asked for it explicitly).
    pub stream: bool,
    /// Never retry a 429 (the repair attempt: at most one retry per request overall).
    pub no_retry: bool,
}

#[derive(Debug)]
pub struct Outcome {
    pub result: Result<String, AssistError>,
    /// Reported usage summed over attempts (unknown stays `None`).
    pub usage: Usage,
    /// Provider attempts made (each counts toward the request limit).
    pub attempts: u32,
    pub finish_reason: Option<String>,
    /// The reply came over a stream that reached its terminal event.
    pub streamed: bool,
    /// Native structured output was requested on the wire.
    pub native: bool,
    /// HTTP status of the last response, if any.
    pub status: Option<u16>,
}

fn err(c: Category, m: impl Into<String>) -> AssistError {
    AssistError::new(c, m)
}

/// A model ID that may be placed in a URL path (Gemini).
fn path_safe(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 128
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
}

/// URL, headers and JSON body of one request.
type Built = (String, Vec<(&'static str, String)>, Value);

fn request(
    r: &Resolved,
    key: Option<&str>,
    p: &Payload,
    mode: &Mode,
) -> Result<Built, AssistError> {
    let base = r.endpoint.trim_end_matches('/');
    let schema = mode.native_schema.as_ref();
    Ok(match r.connection.adapter {
        Adapter::Anthropic => {
            let mut body = json!({
                "model": p.model,
                "max_tokens": p.max_output_tokens,
                "system": p.system,
                "messages": [{"role": "user", "content": p.user}],
            });
            if mode.stream {
                body["stream"] = json!(true);
            }
            if let Some(s) = schema {
                body["tools"] = json!([{
                    "name": TOOL_NAME,
                    "description": "Return the result as structured data.",
                    "input_schema": s,
                }]);
                body["tool_choice"] = json!({"type": "tool", "name": TOOL_NAME});
            }
            (
                format!("{base}/v1/messages"),
                [
                    key.map(|k| ("x-api-key", k.to_string())),
                    Some(("anthropic-version", ANTHROPIC_VERSION.to_string())),
                ]
                .into_iter()
                .flatten()
                .collect(),
                body,
            )
        }
        Adapter::OpenaiCompatible => {
            let mut body = json!({
                "model": p.model,
                "max_tokens": p.max_output_tokens,
                "messages": [
                    {"role": "system", "content": p.system},
                    {"role": "user", "content": p.user},
                ],
            });
            if mode.stream {
                body["stream"] = json!(true);
                body["stream_options"] = json!({"include_usage": true});
            }
            if let Some(s) = schema {
                body["response_format"] = json!({
                    "type": "json_schema",
                    "json_schema": {"name": "result", "schema": s, "strict": false},
                });
            }
            (
                format!("{base}/v1/chat/completions"),
                key.map(|k| ("authorization", format!("Bearer {k}")))
                    .into_iter()
                    .collect(),
                body,
            )
        }
        Adapter::Ollama => {
            let mut body = json!({
                "model": p.model,
                "stream": mode.stream,
                "options": {"num_predict": p.max_output_tokens},
                "messages": [
                    {"role": "system", "content": p.system},
                    {"role": "user", "content": p.user},
                ],
            });
            if let Some(s) = schema {
                body["format"] = s.clone();
            }
            (
                format!("{base}/api/chat"),
                key.map(|k| ("authorization", format!("Bearer {k}")))
                    .into_iter()
                    .collect(),
                body,
            )
        }
        Adapter::Gemini => {
            if !path_safe(&p.model) {
                return Err(err(
                    Category::NotConfigured,
                    "the model id has characters that cannot be used with the Gemini adapter",
                ));
            }
            let mut gen_cfg = json!({"maxOutputTokens": p.max_output_tokens});
            if let Some(s) = schema {
                gen_cfg["responseMimeType"] = json!("application/json");
                gen_cfg["responseSchema"] = s.clone();
            }
            let url = if mode.stream {
                format!(
                    "{base}/v1beta/models/{}:streamGenerateContent?alt=sse",
                    p.model
                )
            } else {
                format!("{base}/v1beta/models/{}:generateContent", p.model)
            };
            (
                url,
                key.map(|k| ("x-goog-api-key", k.to_string()))
                    .into_iter()
                    .collect(),
                json!({
                    "systemInstruction": {"parts": [{"text": p.system}]},
                    "contents": [{"role": "user", "parts": [{"text": p.user}]}],
                    "generationConfig": gen_cfg,
                }),
            )
        }
    })
}

fn gemini_finish(f: &str) -> String {
    match f {
        "STOP" => "stop".into(),
        "MAX_TOKENS" => "length".into(),
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" | "LANGUAGE" => {
            "content_filter".into()
        }
        other => other.to_ascii_lowercase(),
    }
}

/// Parse a successful response: (text, usage, finish reason).
fn parse(adapter: Adapter, v: &Value) -> (Option<String>, Usage, Option<String>) {
    let n = |x: &Value| x.as_u64();
    match adapter {
        Adapter::Anthropic => {
            // A forced tool call carries native structured output: its input is the result.
            let tool = v["content"]
                .as_array()
                .and_then(|a| a.iter().find(|b| b["type"] == "tool_use"))
                .map(|b| b["input"].to_string());
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
            let usage = Usage {
                input_tokens: n(&v["usage"]["input_tokens"]),
                output_tokens: n(&v["usage"]["output_tokens"]),
            };
            let stop = v["stop_reason"].as_str().map(str::to_string);
            // `tool_use` is the normal end of a forced tool call, not a failure.
            let stop = stop.map(|s| if s == "tool_use" { "stop".into() } else { s });
            (
                tool.or_else(|| v["content"].is_array().then_some(text)),
                usage,
                stop,
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
        Adapter::Gemini => {
            let parts = v["candidates"][0]["content"]["parts"].as_array();
            let text: Option<String> = parts.map(|a| {
                a.iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("")
            });
            let finish = v["candidates"][0]["finishReason"]
                .as_str()
                .map(gemini_finish)
                .or_else(|| {
                    v["promptFeedback"]["blockReason"]
                        .is_string()
                        .then(|| "content_filter".to_string())
                });
            (
                text,
                Usage {
                    input_tokens: n(&v["usageMetadata"]["promptTokenCount"]),
                    output_tokens: n(&v["usageMetadata"]["candidatesTokenCount"]),
                },
                finish,
            )
        }
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

pub(crate) fn status_error(status: u16) -> AssistError {
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

pub(crate) fn client() -> Result<reqwest::Client, AssistError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("vibeke-assist/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| err(Category::ProviderUnavailable, "HTTP client unavailable"))
}

pub(crate) async fn read_bounded(mut resp: reqwest::Response) -> Result<Vec<u8>, AssistError> {
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

// ---- streaming ------------------------------------------------------------------------------

/// Incremental decoder for the providers' streaming formats: server-sent events (Anthropic,
/// OpenAI-compatible, Gemini) and newline-delimited JSON (Ollama). Feed raw bytes; text
/// deltas go to the sink as they complete. [`StreamDecoder::finish`] fails when the stream
/// ended without its terminal event.
pub struct StreamDecoder {
    adapter: Adapter,
    buf: Vec<u8>,
    text: String,
    usage: Usage,
    finish: Option<String>,
    ended: bool,
    tool_mode: bool,
}

impl StreamDecoder {
    pub fn new(adapter: Adapter) -> Self {
        StreamDecoder {
            adapter,
            buf: Vec::new(),
            text: String::new(),
            usage: Usage::default(),
            finish: None,
            ended: false,
            tool_mode: false,
        }
    }

    pub fn feed(&mut self, chunk: &[u8], sink: &mut dyn FnMut(&str)) -> Result<(), AssistError> {
        self.buf.extend_from_slice(chunk);
        if self.buf.len() > 4 * MAX_BODY {
            return Err(err(
                Category::InvalidOutput,
                "provider stream line too long",
            ));
        }
        while let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            let line = String::from_utf8_lossy(&line);
            self.line(line.trim_end_matches(['\n', '\r']), sink)?;
        }
        Ok(())
    }

    fn push(&mut self, t: &str, sink: &mut dyn FnMut(&str)) -> Result<(), AssistError> {
        if t.is_empty() {
            return Ok(());
        }
        if self.text.len() + t.len() > MAX_BODY {
            return Err(err(Category::InvalidOutput, "provider response too large"));
        }
        self.text.push_str(t);
        sink(t);
        Ok(())
    }

    fn line(&mut self, line: &str, sink: &mut dyn FnMut(&str)) -> Result<(), AssistError> {
        let line = line.trim();
        if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
            return Ok(());
        }
        let payload = if self.adapter == Adapter::Ollama {
            line
        } else if let Some(d) = line.strip_prefix("data:") {
            d.trim()
        } else {
            return Ok(());
        };
        if payload == "[DONE]" {
            self.ended = true;
            return Ok(());
        }
        let v: Value = serde_json::from_str(payload).map_err(|_| {
            err(
                Category::InvalidOutput,
                "the provider's stream is malformed",
            )
        })?;
        self.apply(&v, sink)
    }

    fn apply(&mut self, v: &Value, sink: &mut dyn FnMut(&str)) -> Result<(), AssistError> {
        let n = |x: &Value| x.as_u64();
        match self.adapter {
            Adapter::Anthropic => match v["type"].as_str().unwrap_or("") {
                "message_start" => {
                    self.usage.input_tokens = n(&v["message"]["usage"]["input_tokens"]);
                    self.usage.output_tokens = n(&v["message"]["usage"]["output_tokens"]);
                }
                "content_block_start" => {
                    if v["content_block"]["type"] == "tool_use" {
                        self.tool_mode = true;
                    }
                }
                "content_block_delta" => match v["delta"]["type"].as_str().unwrap_or("") {
                    "text_delta" if !self.tool_mode => {
                        if let Some(t) = v["delta"]["text"].as_str() {
                            self.push(t, sink)?;
                        }
                    }
                    "input_json_delta" => {
                        if let Some(t) = v["delta"]["partial_json"].as_str() {
                            self.push(t, sink)?;
                        }
                    }
                    _ => {}
                },
                "message_delta" => {
                    if let Some(s) = v["delta"]["stop_reason"].as_str() {
                        self.finish = Some(if s == "tool_use" {
                            "stop".into()
                        } else {
                            s.into()
                        });
                    }
                    if let Some(o) = n(&v["usage"]["output_tokens"]) {
                        self.usage.output_tokens = Some(o);
                    }
                    if let Some(i) = n(&v["usage"]["input_tokens"]) {
                        self.usage.input_tokens = Some(i);
                    }
                }
                "message_stop" => self.ended = true,
                "error" => {
                    return Err(match v["error"]["type"].as_str().unwrap_or("") {
                        "overloaded_error" | "rate_limit_error" => err(
                            Category::RateLimited,
                            "the provider reported overload during the stream",
                        ),
                        "authentication_error" | "permission_error" => err(
                            Category::AuthenticationFailed,
                            "the provider rejected the credential during the stream",
                        ),
                        _ => err(
                            Category::ProviderUnavailable,
                            "the provider reported an error during the stream",
                        ),
                    });
                }
                _ => {}
            },
            Adapter::OpenaiCompatible => {
                if v["error"].is_object() {
                    return Err(err(
                        Category::ProviderUnavailable,
                        "the provider reported an error during the stream",
                    ));
                }
                if v["usage"].is_object() {
                    if let Some(i) = n(&v["usage"]["prompt_tokens"]) {
                        self.usage.input_tokens = Some(i);
                    }
                    if let Some(o) = n(&v["usage"]["completion_tokens"]) {
                        self.usage.output_tokens = Some(o);
                    }
                }
                if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
                    self.push(t, sink)?;
                }
                if let Some(f) = v["choices"][0]["finish_reason"].as_str() {
                    self.finish = Some(f.to_string());
                }
            }
            Adapter::Ollama => {
                if v["error"].is_string() {
                    return Err(err(
                        Category::ProviderUnavailable,
                        "the provider reported an error during the stream",
                    ));
                }
                if let Some(t) = v["message"]["content"].as_str() {
                    self.push(t, sink)?;
                }
                if v["done"] == true {
                    self.ended = true;
                    self.finish = v["done_reason"].as_str().map(str::to_string);
                    self.usage.input_tokens = n(&v["prompt_eval_count"]);
                    self.usage.output_tokens = n(&v["eval_count"]);
                }
            }
            Adapter::Gemini => {
                if v["error"].is_object() {
                    return Err(err(
                        Category::ProviderUnavailable,
                        "the provider reported an error during the stream",
                    ));
                }
                if let Some(parts) = v["candidates"][0]["content"]["parts"].as_array() {
                    for p in parts {
                        if let Some(t) = p["text"].as_str() {
                            self.push(t, sink)?;
                        }
                    }
                }
                if let Some(i) = n(&v["usageMetadata"]["promptTokenCount"]) {
                    self.usage.input_tokens = Some(i);
                }
                if let Some(o) = n(&v["usageMetadata"]["candidatesTokenCount"]) {
                    self.usage.output_tokens = Some(o);
                }
                if let Some(f) = v["candidates"][0]["finishReason"].as_str() {
                    self.finish = Some(gemini_finish(f));
                    self.ended = true;
                } else if v["promptFeedback"]["blockReason"].is_string() {
                    self.finish = Some("content_filter".into());
                    self.ended = true;
                }
            }
        }
        Ok(())
    }

    /// The accumulated text, usage and finish reason, or an error when the stream ended before
    /// its terminal event (a cut-off stream is never a shorter reply).
    pub fn finish(mut self) -> Result<(String, Usage, Option<String>), AssistError> {
        if !self.buf.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).to_string();
            let mut ignore = |_: &str| {};
            self.line(&rest, &mut ignore)?;
        }
        if !self.ended {
            return Err(err(
                Category::ProviderUnavailable,
                "the provider's stream ended before it finished",
            ));
        }
        Ok((self.text, self.usage, self.finish))
    }
}

// ---- tool calls (contract only; Vibeke executes none) ------------------------------------------

/// A model-requested tool call, decoded for the future tool contract (14 §11 A0). The first
/// feature has no model-callable tools and nothing in Vibeke executes these.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

/// Decode the tool calls of a non-streamed response. Arguments always come back as a JSON
/// value: OpenAI-compatible providers send them as a JSON *string*, the others as an object.
pub fn decode_tool_calls(adapter: Adapter, v: &Value) -> Result<Vec<ToolCall>, AssistError> {
    let bad = || {
        err(
            Category::InvalidOutput,
            "a tool call has malformed arguments",
        )
    };
    let mut out = vec![];
    match adapter {
        Adapter::Anthropic => {
            for b in v["content"].as_array().into_iter().flatten() {
                if b["type"] == "tool_use" {
                    out.push(ToolCall {
                        id: b["id"].as_str().map(str::to_string),
                        name: b["name"].as_str().ok_or_else(bad)?.to_string(),
                        arguments: b["input"].clone(),
                    });
                }
            }
        }
        Adapter::OpenaiCompatible => {
            for c in v["choices"][0]["message"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let f = &c["function"];
                let args = match &f["arguments"] {
                    Value::String(s) => serde_json::from_str(s).map_err(|_| bad())?,
                    Value::Object(_) => f["arguments"].clone(),
                    _ => return Err(bad()),
                };
                out.push(ToolCall {
                    id: c["id"].as_str().map(str::to_string),
                    name: f["name"].as_str().ok_or_else(bad)?.to_string(),
                    arguments: args,
                });
            }
        }
        Adapter::Ollama => {
            for c in v["message"]["tool_calls"].as_array().into_iter().flatten() {
                let f = &c["function"];
                out.push(ToolCall {
                    id: None,
                    name: f["name"].as_str().ok_or_else(bad)?.to_string(),
                    arguments: match &f["arguments"] {
                        Value::String(s) => serde_json::from_str(s).map_err(|_| bad())?,
                        a @ Value::Object(_) => a.clone(),
                        _ => return Err(bad()),
                    },
                });
            }
        }
        Adapter::Gemini => {
            for p in v["candidates"][0]["content"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
            {
                if let Some(fc) = p.get("functionCall") {
                    out.push(ToolCall {
                        id: None,
                        name: fc["name"].as_str().ok_or_else(bad)?.to_string(),
                        arguments: fc["args"].clone(),
                    });
                }
            }
        }
    }
    Ok(out)
}

// ---- generation -------------------------------------------------------------------------------

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
    gate: impl FnMut(u32) -> Result<(), AssistError>,
) -> Outcome {
    generate_with(r, key, p, deadline, &Mode::default(), gate, None).await
}

/// The general entry point: [`Mode`] selects native structured output and streaming; text
/// deltas of a streamed reply go to `sink` as they arrive (nothing is sent to it after the
/// call returns or is dropped).
pub async fn generate_with(
    r: &Resolved,
    key: Option<&str>,
    p: &Payload,
    deadline: Instant,
    mode: &Mode,
    mut gate: impl FnMut(u32) -> Result<(), AssistError>,
    mut sink: Option<&mut (dyn FnMut(&str) + Send)>,
) -> Outcome {
    let mut out = Outcome {
        result: Err(err(Category::ProviderUnavailable, "not attempted")),
        usage: Usage::default(),
        attempts: 0,
        finish_reason: None,
        streamed: false,
        native: mode.native_schema.is_some(),
        status: None,
    };
    let http = match client() {
        Ok(c) => c,
        Err(e) => {
            out.result = Err(e);
            return out;
        }
    };
    let (url, headers, body) = match request(r, key, p, mode) {
        Ok(x) => x,
        Err(e) => {
            out.result = Err(e);
            return out;
        }
    };
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
        out.status = Some(status);
        if status == 429 && out.attempts == 1 && !mode.no_retry {
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
        if mode.stream {
            return stream_body(r.connection.adapter, resp, out, sink.take()).await;
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

/// Read a streamed 2xx body to its terminal event. Never retried: content may have been
/// delivered to the sink already.
async fn stream_body(
    adapter: Adapter,
    mut resp: reqwest::Response,
    mut out: Outcome,
    mut sink: Option<&mut (dyn FnMut(&str) + Send)>,
) -> Outcome {
    let mut dec = StreamDecoder::new(adapter);
    let mut nothing = |_: &str| {};
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let s: &mut dyn FnMut(&str) = match sink.as_deref_mut() {
                    Some(s) => s,
                    None => &mut nothing,
                };
                if let Err(e) = dec.feed(&chunk, s) {
                    out.result = Err(e);
                    return out;
                }
            }
            Ok(None) => break,
            Err(e) if e.is_timeout() => {
                out.result = Err(err(Category::Timeout, "request deadline exceeded"));
                return out;
            }
            Err(_) => {
                out.result = Err(err(
                    Category::ProviderUnavailable,
                    "the provider's stream was interrupted",
                ));
                return out;
            }
        }
    }
    match dec.finish() {
        Ok((text, usage, finish)) => {
            out.usage.add(usage);
            out.streamed = true;
            out.finish_reason = finish.clone();
            out.result = match finish.as_deref().and_then(finish_error) {
                Some(e) => Err(e),
                None if text.is_empty() => Err(err(
                    Category::InvalidOutput,
                    "provider response has no text content",
                )),
                None => Ok(text),
            };
        }
        Err(e) => out.result = Err(e),
    }
    out
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

    // ---- Gemini, native structured output, streaming, tool-call contract ------------------------

    fn stream_mode() -> Mode {
        Mode {
            native_schema: None,
            stream: true,
            no_retry: false,
        }
    }

    async fn streamed(
        adapter: &str,
        reply: Reply,
        mode: &Mode,
    ) -> (Outcome, Vec<String>, FakeServer) {
        let f = FakeServer::start(vec![reply]).await;
        let mut deltas: Vec<String> = vec![];
        let mut sink = |t: &str| deltas.push(t.to_string());
        let o = generate_with(
            &resolved(adapter, &f.url()),
            Some("k"),
            &payload(),
            soon(),
            mode,
            |_| Ok(()),
            Some(&mut sink),
        )
        .await;
        (o, deltas, f)
    }

    #[tokio::test]
    async fn gemini_shape_usage_and_finish_reasons() {
        let f = FakeServer::start(vec![
            Reply::gemini("{\"title\":\"t\"}", 21, 8),
            Reply::json(json!({
                "candidates": [{"content": {"parts": [{"text": "cut"}]}, "finishReason": "MAX_TOKENS"}],
                "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 9},
            })),
            Reply::json(json!({
                "candidates": [{"finishReason": "SAFETY"}],
                "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 0},
            })),
            Reply::json(json!({"promptFeedback": {"blockReason": "SAFETY"}})),
        ])
        .await;
        let r = resolved("gemini", &f.url());
        let o = generate(&r, Some("gk"), &payload(), soon()).await;
        assert_eq!(o.result.unwrap(), "{\"title\":\"t\"}");
        assert_eq!(o.usage.input_tokens, Some(21));
        assert_eq!(o.usage.output_tokens, Some(8));
        assert_eq!(o.finish_reason.as_deref(), Some("stop"));
        let req = &f.requests()[0];
        assert_eq!(req.path, "/v1beta/models/m-1:generateContent");
        assert_eq!(req.header("x-goog-api-key").as_deref(), Some("gk"));
        assert!(req.header("authorization").is_none());
        assert_eq!(req.json()["systemInstruction"]["parts"][0]["text"], "sys");
        assert_eq!(req.json()["contents"][0]["parts"][0]["text"], "hello");
        assert_eq!(req.json()["generationConfig"]["maxOutputTokens"], 100);
        assert!(
            req.json()["generationConfig"]
                .get("responseSchema")
                .is_none()
        );
        let e = generate(&r, Some("gk"), &payload(), soon())
            .await
            .result
            .unwrap_err();
        assert!(e.message.contains("truncated"), "{e}");
        let e = generate(&r, Some("gk"), &payload(), soon())
            .await
            .result
            .unwrap_err();
        assert!(e.message.contains("refused"), "{e}");
        let e = generate(&r, Some("gk"), &payload(), soon())
            .await
            .result
            .unwrap_err();
        assert!(e.message.contains("refused"), "{e}");
    }

    #[tokio::test]
    async fn gemini_refuses_model_ids_that_would_alter_the_url() {
        let f = FakeServer::start(vec![Reply::gemini("x", 1, 1)]).await;
        let mut p = payload();
        p.model = "m/../../admin?x=1".into();
        let o = generate(&resolved("gemini", &f.url()), Some("k"), &p, soon()).await;
        assert_eq!(o.result.unwrap_err().category, Category::NotConfigured);
        assert_eq!(o.attempts, 0);
        assert_eq!(f.count(), 0);
    }

    #[tokio::test]
    async fn native_structured_output_wire_shapes() {
        let schema = json!({"type": "object", "properties": {"title": {"type": "string"}}});
        let mode = Mode {
            native_schema: Some(schema.clone()),
            ..Default::default()
        };
        // Anthropic: a forced tool; the tool input is the result.
        let f = FakeServer::start(vec![Reply::anthropic_tool(json!({"title": "t"}), 7, 3)]).await;
        let o = generate_with(
            &resolved("anthropic", &f.url()),
            Some("k"),
            &payload(),
            soon(),
            &mode,
            |_| Ok(()),
            None,
        )
        .await;
        assert!(o.native);
        assert_eq!(o.finish_reason.as_deref(), Some("stop"));
        let v: Value = serde_json::from_str(&o.result.unwrap()).unwrap();
        assert_eq!(v["title"], "t");
        let b = f.requests()[0].json();
        assert_eq!(b["tools"][0]["name"], TOOL_NAME);
        assert_eq!(b["tools"][0]["input_schema"], schema);
        assert_eq!(b["tool_choice"]["type"], "tool");
        assert_eq!(b["messages"][0]["content"], "hello");
        // OpenAI-compatible: response_format json_schema.
        let f = FakeServer::start(vec![Reply::openai("{\"title\":\"t\"}", 1, 1)]).await;
        let o = generate_with(
            &resolved("openai_compatible", &f.url()),
            Some("k"),
            &payload(),
            soon(),
            &mode,
            |_| Ok(()),
            None,
        )
        .await;
        assert!(o.result.is_ok());
        let b = f.requests()[0].json();
        assert_eq!(b["response_format"]["type"], "json_schema");
        assert_eq!(b["response_format"]["json_schema"]["schema"], schema);
        // Ollama: `format` carries the schema.
        let f = FakeServer::start(vec![Reply::ollama("{\"title\":\"t\"}", 1, 1)]).await;
        let o = generate_with(
            &resolved("ollama", &f.url()),
            None,
            &payload(),
            soon(),
            &mode,
            |_| Ok(()),
            None,
        )
        .await;
        assert!(o.result.is_ok());
        assert_eq!(f.requests()[0].json()["format"], schema);
        // Gemini: responseMimeType + responseSchema.
        let f = FakeServer::start(vec![Reply::gemini("{\"title\":\"t\"}", 1, 1)]).await;
        let o = generate_with(
            &resolved("gemini", &f.url()),
            Some("k"),
            &payload(),
            soon(),
            &mode,
            |_| Ok(()),
            None,
        )
        .await;
        assert!(o.result.is_ok());
        let g = &f.requests()[0].json()["generationConfig"];
        assert_eq!(g["responseMimeType"], "application/json");
        assert_eq!(g["responseSchema"], schema);
        // Without a schema none of those fields is sent.
        let f = FakeServer::start(vec![Reply::anthropic("{}", 1, 1)]).await;
        let _ = generate(
            &resolved("anthropic", &f.url()),
            Some("k"),
            &payload(),
            soon(),
        )
        .await;
        let b = f.requests()[0].json();
        assert!(b.get("tools").is_none() && b.get("tool_choice").is_none());
    }

    #[tokio::test]
    async fn streaming_delivers_deltas_and_usage_for_every_adapter() {
        for (adapter, reply, path, key_header) in [
            (
                "anthropic",
                Reply::anthropic_stream(&["{\"ti", "tle\":", "\"t\"}"], 11, 7),
                "/v1/messages",
                "x-api-key",
            ),
            (
                "openai_compatible",
                Reply::openai_stream(&["{\"ti", "tle\":", "\"t\"}"], 11, 7),
                "/v1/chat/completions",
                "authorization",
            ),
            (
                "ollama",
                Reply::ollama_stream(&["{\"ti", "tle\":", "\"t\"}"], 11, 7),
                "/api/chat",
                "authorization",
            ),
            (
                "gemini",
                Reply::gemini_stream(&["{\"ti", "tle\":", "\"t\"}"], 11, 7),
                "/v1beta/models/m-1:streamGenerateContent?alt=sse",
                "x-goog-api-key",
            ),
        ] {
            let (o, deltas, f) = streamed(adapter, reply, &stream_mode()).await;
            assert_eq!(
                o.result.as_deref().unwrap(),
                "{\"title\":\"t\"}",
                "{adapter}"
            );
            assert!(o.streamed, "{adapter}");
            assert_eq!(deltas, vec!["{\"ti", "tle\":", "\"t\"}"], "{adapter}");
            assert_eq!(o.usage.input_tokens, Some(11), "{adapter}");
            assert_eq!(o.usage.output_tokens, Some(7), "{adapter}");
            assert_eq!(o.attempts, 1);
            let req = &f.requests()[0];
            assert_eq!(req.path, path, "{adapter}");
            assert!(req.header(key_header).is_some(), "{adapter}");
            if adapter != "gemini" {
                assert_eq!(
                    req.json()["stream"],
                    json!(true),
                    "{adapter}: stream flag on the wire"
                );
            }
        }
    }

    #[tokio::test]
    async fn streaming_native_tool_input_arrives_as_json_text() {
        let pieces = vec![
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":1}}}\n\n".to_string(),
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"emit_result\",\"input\":{}}}\n\n".to_string(),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"title\\\":\"}}\n\n".to_string(),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"t\\\"}\"}}\n\n".to_string(),
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":9}}\n\n".to_string(),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
        ];
        let mode = Mode {
            native_schema: Some(json!({"type": "object"})),
            stream: true,
            ..Default::default()
        };
        let (o, deltas, _) = streamed("anthropic", Reply::sse(pieces), &mode).await;
        assert_eq!(o.result.as_deref().unwrap(), "{\"title\":\"t\"}");
        assert_eq!(deltas.concat(), "{\"title\":\"t\"}");
        assert_eq!(o.finish_reason.as_deref(), Some("stop"));
    }

    #[tokio::test]
    async fn a_stream_cut_before_its_end_is_a_failure_and_is_not_retried() {
        for (adapter, reply) in [
            (
                "anthropic",
                Reply::anthropic_stream(&["a", "b"], 5, 5).truncated(2),
            ),
            (
                "openai_compatible",
                Reply::openai_stream(&["a", "b"], 5, 5).truncated(3),
            ),
            (
                "ollama",
                Reply::ollama_stream(&["a", "b"], 5, 5).truncated(1),
            ),
            (
                "gemini",
                Reply::gemini_stream(&["a", "b"], 5, 5).truncated(1),
            ),
        ] {
            let (o, deltas, f) = streamed(adapter, reply, &stream_mode()).await;
            let e = o.result.unwrap_err();
            assert_eq!(e.category, Category::ProviderUnavailable, "{adapter}: {e}");
            assert!(!o.streamed, "{adapter}");
            assert!(
                !deltas.is_empty(),
                "{adapter}: deltas were delivered before the cut"
            );
            assert_eq!(o.attempts, 1, "{adapter}");
            assert_eq!(
                f.count(),
                1,
                "{adapter}: a partially delivered stream is never retried"
            );
        }
    }

    #[tokio::test]
    async fn stream_errors_and_malformed_events_map_to_categories() {
        let overloaded = Reply::sse(vec![
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"secret echoed\"}}\n\n".to_string(),
        ]);
        let (o, _, _) = streamed("anthropic", overloaded, &stream_mode()).await;
        let e = o.result.unwrap_err();
        assert_eq!(e.category, Category::RateLimited);
        assert!(!e.message.contains("secret"));
        let bad = Reply::sse(vec!["data: {not json\n\n".to_string()]);
        let (o, _, _) = streamed("openai_compatible", bad, &stream_mode()).await;
        assert_eq!(o.result.unwrap_err().category, Category::InvalidOutput);
        // A truncated reply over a stream is still reported as truncated.
        let long = Reply::sse(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":\"length\"}]}\n\n".into(),
            "data: [DONE]\n\n".into(),
        ]);
        let (o, _, _) = streamed("openai_compatible", long, &stream_mode()).await;
        assert!(o.result.unwrap_err().message.contains("truncated"));
        assert!(o.streamed);
        // An HTTP error before the stream starts is categorized like any other.
        let (o, _, _) = streamed(
            "anthropic",
            Reply::status(401, "{\"error\":\"echoed prompt\"}"),
            &stream_mode(),
        )
        .await;
        assert_eq!(
            o.result.unwrap_err().category,
            Category::AuthenticationFailed
        );
    }

    #[tokio::test]
    async fn a_slow_stream_hits_the_total_deadline_and_stops_delivering() {
        let f = FakeServer::start(vec![
            Reply::anthropic_stream(&["a", "b", "c", "d"], 5, 5)
                .with_piece_delay(Duration::from_millis(400)),
        ])
        .await;
        let mut deltas: Vec<String> = vec![];
        let mut sink = |t: &str| deltas.push(t.to_string());
        let t = Instant::now();
        let o = generate_with(
            &resolved("anthropic", &f.url()),
            Some("k"),
            &payload(),
            Instant::now() + Duration::from_millis(900),
            &stream_mode(),
            |_| Ok(()),
            Some(&mut sink),
        )
        .await;
        assert_eq!(o.result.unwrap_err().category, Category::Timeout);
        assert!(t.elapsed() < Duration::from_secs(3));
        let n = deltas.len();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(deltas.len(), n, "nothing is published after the deadline");
        assert!(n < 4);
    }

    #[test]
    fn decoder_handles_arbitrary_chunk_boundaries() {
        let r = Reply::anthropic_stream(&["héllo ", "wörld"], 3, 4);
        let all: String = r.pieces.unwrap().concat();
        for step in [1usize, 2, 3, 7, 64] {
            let mut dec = StreamDecoder::new(Adapter::Anthropic);
            let mut got = vec![];
            let mut sink = |t: &str| got.push(t.to_string());
            for c in all.as_bytes().chunks(step) {
                dec.feed(c, &mut sink).unwrap();
            }
            let (text, usage, finish) = dec.finish().unwrap();
            assert_eq!(text, "héllo wörld", "step {step}");
            assert_eq!(got.concat(), "héllo wörld", "step {step}");
            assert_eq!(usage.input_tokens, Some(3));
            assert_eq!(usage.output_tokens, Some(4));
            assert_eq!(finish.as_deref(), Some("end_turn"));
        }
    }

    #[test]
    fn tool_call_arguments_decode_for_every_adapter() {
        // Anthropic: `input` is an object.
        let v = json!({"content": [
            {"type": "text", "text": "calling"},
            {"type": "tool_use", "id": "tu_1", "name": "pane_read", "input": {"pane": "p1", "lines": 40}},
        ]});
        let c = decode_tool_calls(Adapter::Anthropic, &v).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].id.as_deref(), Some("tu_1"));
        assert_eq!(c[0].name, "pane_read");
        assert_eq!(c[0].arguments, json!({"pane": "p1", "lines": 40}));
        // OpenAI-compatible: `arguments` is a JSON *string*.
        let v = json!({"choices": [{"message": {"tool_calls": [
            {"id": "call_1", "type": "function", "function": {"name": "pane_read", "arguments": "{\"pane\":\"p1\",\"lines\":40}"}},
            {"id": "call_2", "type": "function", "function": {"name": "noop", "arguments": "{}"}},
        ]}}]});
        let c = decode_tool_calls(Adapter::OpenaiCompatible, &v).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].arguments, json!({"pane": "p1", "lines": 40}));
        assert_eq!(c[1].arguments, json!({}));
        // Malformed argument strings fail instead of being guessed at.
        let bad = json!({"choices": [{"message": {"tool_calls": [
            {"id": "c", "function": {"name": "x", "arguments": "{\"pane\":"}},
        ]}}]});
        assert_eq!(
            decode_tool_calls(Adapter::OpenaiCompatible, &bad)
                .unwrap_err()
                .category,
            Category::InvalidOutput
        );
        // Ollama: arguments are an object.
        let v = json!({"message": {"tool_calls": [{"function": {"name": "pane_read", "arguments": {"pane": "p1"}}}]}});
        let c = decode_tool_calls(Adapter::Ollama, &v).unwrap();
        assert_eq!(c[0].arguments["pane"], "p1");
        // Gemini: `functionCall.args`.
        let v = json!({"candidates": [{"content": {"parts": [
            {"text": "x"},
            {"functionCall": {"name": "pane_read", "args": {"pane": "p1"}}},
        ]}}]});
        let c = decode_tool_calls(Adapter::Gemini, &v).unwrap();
        assert_eq!(c[0].name, "pane_read");
        assert_eq!(c[0].arguments, json!({"pane": "p1"}));
        // No calls is an empty list, and nothing here executes anything.
        assert!(
            decode_tool_calls(Adapter::Anthropic, &json!({"content": []}))
                .unwrap()
                .is_empty()
        );
    }
}
