//! An authenticated render stream for the browser's WebAssembly TUI.
//!
//! This is a full host client. Restricted devices never reach the server socket. The bridge
//! belongs to one encrypted device connection and is dropped on disconnect or revocation.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use vk_e2e::b64;
use vk_proto::frame::{self, asyncio};
use vk_proto::render::{ClientFrame, ServerFrame};

use crate::Gateway;
use crate::api::ApiError;
use crate::session::Out;
use crate::state::{Device, Scope};

const MAX_INPUT: usize = 512 * 1024;
const CHUNK: usize = 48 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(1);

struct ReaderTask(tokio::task::JoinHandle<()>);
impl Drop for ReaderTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub struct Bridge {
    pub id: String,
    tx: mpsc::Sender<Vec<ClientFrame>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn authorize(device: &Device) -> Result<(), ApiError> {
    if device.kind != "device"
        || device.scope != Scope::Full
        || device.limit.is_some()
        || device.expired()
    {
        return Err(ApiError::new(
            "forbidden",
            "The TUI needs a paired device with full host access",
        ));
    }
    Ok(())
}

impl Bridge {
    pub async fn attach(
        gw: Arc<Gateway>,
        device: &Device,
        out: Out,
        protocol: u64,
    ) -> Result<(Self, Value), ApiError> {
        authorize(device)?;
        if protocol != vk_proto::render::PROTOCOL as u64 {
            return Err(ApiError::new(
                "unsupported",
                "The browser TUI and host use different render protocols. Rebuild the browser app and update the host together.",
            ));
        }
        let id = format!(
            "web-tui-{}-{}",
            device.id,
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let stream = tokio::time::timeout(
            Duration::from_secs(5),
            UnixStream::connect(gw.server.path()),
        )
        .await
        .map_err(|_| ApiError::unavailable("Server connection timed out"))?
        .map_err(|e| ApiError::unavailable(e.to_string()))?;
        let (rd, mut wr) = stream.into_split();
        let mut rd = BufReader::new(rd);
        // Keep the gateway identity on the render stream so server audit and remote redaction
        // also apply to commands from this client. The browser cannot choose this identity.
        let hello = json!({"jsonrpc":"2.0", "id":1, "method":"client.hello", "params":{
            "client":"vibeke-browser-tui", "kind":"gateway", "remote":true, "api":"vibeke/1"}});
        request(&mut rd, &mut wr, hello).await?;
        let reply = request(
            &mut rd,
            &mut wr,
            json!({"jsonrpc":"2.0", "id":2, "method":"render.attach", "params":{
                "client_id":id, "protocol":protocol, "remote":true,
                "caps":{"max_fps":60, "kitty_keyboard":true, "osc52":true, "truecolor":true}
            }}),
        )
        .await?;
        let (tx, mut rx) = mpsc::channel::<Vec<ClientFrame>>(32);
        let device_id = device.id.clone();
        let actor = format!("gateway:{} ({})", device.name, device.id);
        let stream_id = id.clone();
        let task = tokio::spawn(async move {
            // Keep read_exact out of the select: cancelling a partial read loses framing.
            let (frames_tx, mut frames_rx) = mpsc::channel(4);
            let _reader = ReaderTask(tokio::spawn(async move {
                loop {
                    let f = asyncio::read_frame::<_, ServerFrame>(&mut rd).await;
                    let done = f.is_err();
                    if frames_tx.send(f).await.is_err() || done {
                        break;
                    }
                }
            }));
            let mut check = tokio::time::interval(Duration::from_secs(1));
            let result: anyhow::Result<()> = async {
                loop {
                    let allowed = gw.device(&device_id).is_some_and(|d| authorize(&d).is_ok());
                    anyhow::ensure!(allowed, "Device no longer has full host access");
                    tokio::select! {
                        _ = check.tick() => {}
                        frames = rx.recv() => {
                            let Some(frames) = frames else { break; };
                            for mut f in frames {
                                prepare(&mut f, &actor)?;
                                asyncio::write_frame(&mut wr, &f).await?;
                            }
                            wr.flush().await?;
                        }
                        f = frames_rx.recv() => {
                            let f = f.ok_or_else(|| anyhow::anyhow!("Render stream closed"))??;
                            let data = frame::encode(&f)?;
                            for part in data.chunks(CHUNK) {
                                anyhow::ensure!(out.notify("tui.frame", json!({"stream":stream_id, "data":b64::encode(part)})).await, "Device disconnected");
                            }
                        }
                    }
                }
                Ok(())
            }.await;
            let reason = result
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "TUI closed".into());
            out.notify("tui.closed", json!({"stream":stream_id, "reason":reason}))
                .await;
        });
        let result = json!({"stream":id, "client_id":id, "protocol":protocol, "features":reply.get("features").cloned().unwrap_or(json!([]))});
        Ok((Self { id, tx, task }, result))
    }

    pub fn send(&self, params: &Value) -> Result<(), ApiError> {
        if params.get("stream").and_then(Value::as_str) != Some(&self.id) {
            return Err(ApiError::invalid("Unknown TUI stream"));
        }
        let data = params
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::invalid("Missing TUI input"))?;
        let frames = decode_input(data)?;
        self.tx
            .try_send(frames)
            .map_err(|_| ApiError::unavailable("TUI input queue is full or closed"))
    }
}

async fn request<R: tokio::io::AsyncBufRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(
    rd: &mut R,
    wr: &mut W,
    request: Value,
) -> Result<Value, ApiError> {
    let operation = async {
        wr.write_all(format!("{request}\n").as_bytes()).await?;
        wr.flush().await?;
        let mut line = String::new();
        rd.read_line(&mut line).await?;
        Ok::<_, std::io::Error>(line)
    };
    let line = tokio::time::timeout(Duration::from_secs(5), operation)
        .await
        .map_err(|_| ApiError::unavailable("TUI attach timed out"))?
        .map_err(|e| ApiError::unavailable(e.to_string()))?;
    let v: Value = serde_json::from_str(&line)
        .map_err(|_| ApiError::unavailable("Invalid server response"))?;
    if let Some(error) = v.get("error") {
        return Err(ApiError::unavailable(
            error["message"].as_str().unwrap_or("TUI attach refused"),
        ));
    }
    v.get("result")
        .cloned()
        .ok_or_else(|| ApiError::unavailable("Missing server response"))
}

fn decode_input(data: &str) -> Result<Vec<ClientFrame>, ApiError> {
    if data.len() > MAX_INPUT * 4 / 3 + 4 {
        return Err(ApiError::invalid("TUI input is too large"));
    }
    let data = b64::decode(data).map_err(|_| ApiError::invalid("Invalid TUI input encoding"))?;
    if data.len() > MAX_INPUT {
        return Err(ApiError::invalid("TUI input is too large"));
    }
    let mut cursor = std::io::Cursor::new(&data);
    let mut frames = Vec::new();
    while cursor.position() < data.len() as u64 {
        if frames.len() == 128 {
            return Err(ApiError::invalid("Too many TUI input frames"));
        }
        // Check the declared length before the decoder allocates its body.
        let at = cursor.position() as usize;
        if data.len() - at < 4 {
            return Err(ApiError::invalid("Truncated TUI input"));
        }
        let len = u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        if len > data.len() - at - 4 {
            return Err(ApiError::invalid("Truncated TUI input"));
        }
        frames.push(
            frame::read_frame(&mut cursor).map_err(|_| ApiError::invalid("Invalid TUI frame"))?,
        );
    }
    Ok(frames)
}

fn prepare(frame: &mut ClientFrame, actor: &str) -> anyhow::Result<()> {
    if let ClientFrame::Command { json, .. } = frame {
        let mut v: Value = serde_json::from_str(json)?;
        let object = v
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Invalid TUI command"))?;
        let params = object.entry("params").or_insert_with(|| json!({}));
        let params = params
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Invalid TUI command parameters"))?;
        params.insert("actor".into(), actor.into());
        *json = v.to_string();
    }
    if let ClientFrame::MediaView { shm, .. } = frame {
        *shm = false;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn input_rejects_truncation_and_oversized_length_before_allocation() {
        assert!(decode_input(&b64::encode(&u32::MAX.to_le_bytes())).is_err());
        assert!(decode_input(&b64::encode(&[1, 2])).is_err());
        let data = frame::encode(&ClientFrame::Ping { nonce: 42 }).unwrap();
        assert!(matches!(
            decode_input(&b64::encode(&data)).unwrap()[0],
            ClientFrame::Ping { nonce: 42 }
        ));
    }
    #[test]
    fn command_actor_cannot_be_spoofed() {
        let mut f = ClientFrame::Command {
            req: 1,
            json: json!({"method":"pane.close", "params":{"pane":"p1", "actor":"someone else"}})
                .to_string(),
        };
        prepare(&mut f, "gateway:Browser").unwrap();
        let ClientFrame::Command { json, .. } = f else {
            panic!()
        };
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["params"]["actor"], "gateway:Browser");
    }
    #[test]
    fn only_unrestricted_full_devices_can_attach() {
        let base = json!({"id":"d1", "name":"Browser", "public":"", "scope":"full", "paired_at":0});
        let mut d: Device = serde_json::from_value(base).unwrap();
        assert!(authorize(&d).is_ok());
        for scope in [Scope::View, Scope::Approve] {
            d.scope = scope;
            assert!(authorize(&d).is_err());
        }
        d.scope = Scope::Full;
        for kind in ["share", "peer"] {
            d.kind = kind.into();
            assert!(authorize(&d).is_err());
        }
        d.kind = "device".into();
        d.expires_at = Some(1);
        assert!(authorize(&d).is_err());
    }
}
