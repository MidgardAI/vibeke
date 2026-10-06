//! A local fake HTTP provider for tests (no real model API is ever contacted by Vibeke's test
//! suite). Serves queued replies in order on `127.0.0.1:<random>`, records every request, and
//! can delay replies to exercise deadlines and cancellation.

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
    pub delay: Duration,
    /// Announce a longer body than is sent, then close (a body cut off after the headers).
    pub cut: bool,
    /// A streamed reply: each piece is written and flushed separately (no content length;
    /// the connection closing ends it), with `piece_delay` between pieces. A stream missing
    /// its terminal event simply ends early.
    pub pieces: Option<Vec<String>>,
    pub piece_delay: Duration,
    pub content_type: Option<String>,
}

impl Reply {
    pub fn status(status: u16, body: &str) -> Reply {
        Reply {
            status,
            body: body.into(),
            headers: vec![],
            delay: Duration::ZERO,
            cut: false,
            pieces: None,
            piece_delay: Duration::ZERO,
            content_type: None,
        }
    }
    pub fn json(v: Value) -> Reply {
        Reply::status(200, &v.to_string())
    }
    pub fn anthropic(text: &str, input: u64, output: u64) -> Reply {
        Reply::json(json!({
            "id": "msg_fake", "type": "message", "role": "assistant", "model": "fake",
            "content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": text}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": input, "output_tokens": output},
        }))
    }
    pub fn anthropic_stop(text: &str, stop: &str) -> Reply {
        Reply::json(json!({
            "content": [{"type": "text", "text": text}],
            "stop_reason": stop,
            "usage": {"input_tokens": 5, "output_tokens": 9},
        }))
    }
    pub fn openai(text: &str, input: u64, output: u64) -> Reply {
        Reply::json(json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": input, "completion_tokens": output},
        }))
    }
    pub fn ollama(text: &str, input: u64, output: u64) -> Reply {
        Reply::json(json!({
            "message": {"role": "assistant", "content": text},
            "done": true, "done_reason": "stop",
            "prompt_eval_count": input, "eval_count": output,
        }))
    }
    /// Native Gemini `generateContent` reply.
    pub fn gemini(text: &str, input: u64, output: u64) -> Reply {
        Reply::json(json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": text}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": input, "candidatesTokenCount": output},
        }))
    }
    /// Anthropic reply whose content is a forced tool call (native structured output).
    pub fn anthropic_tool(input_value: Value, input: u64, output: u64) -> Reply {
        Reply::json(json!({
            "content": [{"type": "tool_use", "id": "toolu_fake", "name": "emit_result", "input": input_value}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": input, "output_tokens": output},
        }))
    }
    /// A server-sent-events reply made of `data:` pieces.
    pub fn sse(pieces: Vec<String>) -> Reply {
        let mut r = Reply::status(200, "");
        r.pieces = Some(pieces);
        r.content_type = Some("text/event-stream".into());
        r
    }
    pub fn ndjson(pieces: Vec<String>) -> Reply {
        let mut r = Reply::status(200, "");
        r.pieces = Some(pieces);
        r.content_type = Some("application/x-ndjson".into());
        r
    }
    fn sse_event(name: Option<&str>, v: Value) -> String {
        match name {
            Some(n) => format!("event: {n}\ndata: {v}\n\n"),
            None => format!("data: {v}\n\n"),
        }
    }
    /// Anthropic streaming reply: one text delta per element of `texts`.
    pub fn anthropic_stream(texts: &[&str], input: u64, output: u64) -> Reply {
        let mut p = vec![
            Reply::sse_event(
                Some("message_start"),
                json!({"type": "message_start", "message": {"usage": {"input_tokens": input, "output_tokens": 1}}}),
            ),
            Reply::sse_event(
                Some("content_block_start"),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
        ];
        for t in texts {
            p.push(Reply::sse_event(
                Some("content_block_delta"),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": t}}),
            ));
        }
        p.push(Reply::sse_event(
            Some("content_block_stop"),
            json!({"type": "content_block_stop", "index": 0}),
        ));
        p.push(Reply::sse_event(
            Some("message_delta"),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": output}}),
        ));
        p.push(Reply::sse_event(
            Some("message_stop"),
            json!({"type": "message_stop"}),
        ));
        Reply::sse(p)
    }
    /// OpenAI-compatible streaming reply.
    pub fn openai_stream(texts: &[&str], input: u64, output: u64) -> Reply {
        let mut p: Vec<String> = texts
            .iter()
            .map(|t| {
                Reply::sse_event(
                    None,
                    json!({"choices": [{"index": 0, "delta": {"content": t}, "finish_reason": null}]}),
                )
            })
            .collect();
        p.push(Reply::sse_event(
            None,
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        ));
        p.push(Reply::sse_event(
            None,
            json!({"choices": [], "usage": {"prompt_tokens": input, "completion_tokens": output}}),
        ));
        p.push("data: [DONE]\n\n".into());
        Reply::sse(p)
    }
    /// Ollama streaming reply (newline-delimited JSON).
    pub fn ollama_stream(texts: &[&str], input: u64, output: u64) -> Reply {
        let mut p: Vec<String> = texts
            .iter()
            .map(|t| {
                format!(
                    "{}\n",
                    json!({"message": {"role": "assistant", "content": t}, "done": false})
                )
            })
            .collect();
        p.push(format!(
            "{}\n",
            json!({"message": {"role": "assistant", "content": ""}, "done": true, "done_reason": "stop", "prompt_eval_count": input, "eval_count": output})
        ));
        Reply::ndjson(p)
    }
    /// Gemini streaming reply (`alt=sse`).
    pub fn gemini_stream(texts: &[&str], input: u64, output: u64) -> Reply {
        let n = texts.len();
        let p = texts
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut c = json!({"content": {"role": "model", "parts": [{"text": t}]}});
                let mut v = json!({});
                if i + 1 == n {
                    c["finishReason"] = json!("STOP");
                    v["usageMetadata"] =
                        json!({"promptTokenCount": input, "candidatesTokenCount": output});
                }
                v["candidates"] = json!([c]);
                Reply::sse_event(None, v)
            })
            .collect();
        Reply::sse(p)
    }
    /// Drop the last `n` pieces of a streamed reply (a stream cut before its end marker).
    pub fn truncated(mut self, n: usize) -> Reply {
        if let Some(p) = self.pieces.as_mut() {
            let keep = p.len().saturating_sub(n);
            p.truncate(keep);
        }
        self
    }
    pub fn with_piece_delay(mut self, d: Duration) -> Reply {
        self.piece_delay = d;
        self
    }
    pub fn redirect(location: &str) -> Reply {
        Reply::status(307, "").with_header("location", location)
    }
    pub fn with_header(mut self, k: &str, v: &str) -> Reply {
        self.headers.push((k.into(), v.into()));
        self
    }
    pub fn cut_off(mut self) -> Reply {
        self.cut = true;
        self
    }
    pub fn delayed(mut self, d: Duration) -> Reply {
        self.delay = d;
        self
    }
}

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, k: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.clone())
    }
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

#[derive(Default)]
struct Shared {
    replies: VecDeque<Reply>,
    fallback: Option<Reply>,
    requests: Vec<Recorded>,
}

#[derive(Clone)]
pub struct FakeServer {
    port: u16,
    shared: Arc<Mutex<Shared>>,
}

impl FakeServer {
    /// Start on the current tokio runtime.
    pub async fn start(replies: Vec<Reply>) -> FakeServer {
        let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = l.local_addr().unwrap().port();
        let shared = Arc::new(Mutex::new(Shared {
            replies: replies.into(),
            ..Default::default()
        }));
        let s2 = shared.clone();
        tokio::spawn(async move {
            while let Ok((sock, _)) = l.accept().await {
                tokio::spawn(serve(sock, s2.clone()));
            }
        });
        FakeServer { port, shared }
    }

    /// Start on a dedicated thread with its own runtime (for synchronous tests).
    pub fn start_in_thread(replies: Vec<Reply>) -> FakeServer {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                tx.send(FakeServer::start(replies).await).unwrap();
                std::future::pending::<()>().await;
            });
        });
        rx.recv().unwrap()
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    pub fn push(&self, r: Reply) {
        self.shared.lock().unwrap().replies.push_back(r);
    }
    /// Reply used once the queue is empty (default: HTTP 500).
    pub fn set_fallback(&self, r: Reply) {
        self.shared.lock().unwrap().fallback = Some(r);
    }
    pub fn requests(&self) -> Vec<Recorded> {
        self.shared.lock().unwrap().requests.clone()
    }
    pub fn count(&self) -> usize {
        self.shared.lock().unwrap().requests.len()
    }
}

async fn serve(mut sock: tokio::net::TcpStream, shared: Arc<Mutex<Shared>>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let head_end = loop {
        match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if buf.len() > 1 << 20 {
            return;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        match sock.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
        }
    }
    let reply = {
        let mut s = shared.lock().unwrap();
        s.requests.push(Recorded {
            method,
            path,
            headers,
            body: String::from_utf8_lossy(&body).to_string(),
        });
        s.replies
            .pop_front()
            .or_else(|| s.fallback.clone())
            .unwrap_or_else(|| Reply::status(500, "{\"error\":\"no reply queued\"}"))
    };
    if !reply.delay.is_zero() {
        tokio::time::sleep(reply.delay).await;
    }
    if let Some(pieces) = &reply.pieces {
        let head = format!(
            "HTTP/1.1 {} X\r\ncontent-type: {}\r\nconnection: close\r\ncache-control: no-cache\r\n\r\n",
            reply.status,
            reply.content_type.as_deref().unwrap_or("text/event-stream")
        );
        if sock.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        for piece in pieces {
            if !reply.piece_delay.is_zero() {
                tokio::time::sleep(reply.piece_delay).await;
            }
            if sock.write_all(piece.as_bytes()).await.is_err() || sock.flush().await.is_err() {
                return;
            }
        }
        let _ = sock.shutdown().await;
        return;
    }
    let mut out = format!(
        "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        reply.status,
        reply.body.len() + if reply.cut { 4096 } else { 0 }
    );
    for (k, v) in &reply.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(&reply.body);
    let _ = sock.write_all(out.as_bytes()).await;
    let _ = sock.shutdown().await;
}
