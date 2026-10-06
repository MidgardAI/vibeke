//! A command-line device for testing a gateway without a browser.
//!
//! cargo run -p vk-gateway --example devclient -- pair '<pairing url>' [key-file]
//! cargo run -p vk-gateway --example devclient -- call <key-file> <method> [json-params]

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use vk_e2e::{DeviceKey, Hello, Initiator, PairingLink, b64};

#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    key: DeviceKey,
    relay: String,
    host: String,
    hk: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("pair") => {
            let link = PairingLink::parse(&args[1])?;
            let file = args.get(2).cloned().unwrap_or_else(|| "device.json".into());
            let key = DeviceKey::generate();
            let mut c = open(
                &link.relay,
                &link.host,
                Hello::pair(&link.pid),
                &key,
                &link.host_key()?,
                Some(&link.psk_bytes()?),
            )
            .await?;
            let r = c
                .call(
                    "pair.claim",
                    json!({"name": "devclient", "platform": "cli"}),
                )
                .await?;
            println!("claim: {r}");
            let done = c.recv().await?;
            println!("{done}");
            if done["method"] == "pair.done" {
                let saved = Saved {
                    key,
                    relay: link.relay,
                    host: link.host,
                    hk: link.hk,
                };
                // Holds a private key: create it 0600, never through an existing file or symlink.
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&file)?
                    .write_all(&serde_json::to_vec_pretty(&saved)?)?;
                println!("saved {file}");
            }
        }
        Some("call") => {
            let saved: Saved = serde_json::from_slice(&std::fs::read(&args[1])?)?;
            let mut params: Value = args
                .get(3)
                .map(|p| serde_json::from_str(p))
                .transpose()?
                .unwrap_or(json!({}));
            if params.get("op_id").is_none() {
                params["op_id"] = ulid::Ulid::new().to_string().into();
            }
            let hk = b64::decode_array(&saved.hk)?;
            let mut c = open(
                &saved.relay,
                &saved.host,
                Hello::device(),
                &saved.key,
                &hk,
                None,
            )
            .await?;
            let r = c.call(&args[2], params).await?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        Some("listen") => {
            let saved: Saved = serde_json::from_slice(&std::fs::read(&args[1])?)?;
            let hk = b64::decode_array(&saved.hk)?;
            let mut c = open(
                &saved.relay,
                &saved.host,
                Hello::device(),
                &saved.key,
                &hk,
                None,
            )
            .await?;
            let after: Value = args
                .get(2)
                .map(|a| serde_json::from_str(a))
                .transpose()?
                .unwrap_or(Value::Null);
            let r = c.call("events.subscribe", json!({"after": after})).await?;
            println!("{r}");
            loop {
                println!("{}", c.recv().await?);
            }
        }
        _ => eprintln!(
            "usage: devclient pair <url> [file] | call <file> <method> [json] | listen <file> [after]"
        ),
    }
    Ok(())
}

struct Client {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    session: vk_e2e::Session,
    next: u64,
}

async fn open(
    relay: &str,
    host: &str,
    hello: Hello,
    key: &DeviceKey,
    hk: &[u8; 32],
    psk: Option<&[u8; 32]>,
) -> anyhow::Result<Client> {
    let (mut ws, _) = connect_async(format!("{relay}/v1/connect?host={host}")).await?;
    let hb = hello.to_bytes();
    ws.send(Message::Text(String::from_utf8(hb.clone())?.into()))
        .await?;
    let mut i = Initiator::new(&hb, &key.private, hk, psk)?;
    ws.send(Message::Binary(i.write_first(b"")?.into())).await?;
    match ws.next().await {
        Some(Ok(Message::Binary(m2))) => {
            let (info, session) = i.read_second(&m2)?;
            eprintln!("connected: {}", String::from_utf8_lossy(&info));
            Ok(Client {
                ws,
                session,
                next: 1,
            })
        }
        other => anyhow::bail!("handshake failed: {other:?}"),
    }
}

impl Client {
    async fn recv(&mut self) -> anyhow::Result<Value> {
        loop {
            match tokio::time::timeout(Duration::from_secs(150), self.ws.next()).await? {
                Some(Ok(Message::Binary(b))) => {
                    if let Some(m) = self.session.decrypt(&b)? {
                        return Ok(serde_json::from_slice(&m)?);
                    }
                }
                Some(Ok(_)) => {}
                other => anyhow::bail!("closed: {other:?}"),
            }
        }
    }
    async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next;
        self.next += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        for f in self.session.encrypt(msg.to_string().as_bytes())? {
            self.ws.send(Message::Binary(f.into())).await?;
        }
        loop {
            let m = self.recv().await?;
            if m["id"] == id {
                return Ok(m);
            }
        }
    }
}
