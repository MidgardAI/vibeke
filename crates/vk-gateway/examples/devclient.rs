//! A command-line peer for testing a gateway without a browser, on top of `peer_client`.
//!
//! cargo run -p vk-gateway --example devclient -- pair '<peer or handoff invitation>' [file]
//! cargo run -p vk-gateway --example devclient -- call <file> <method> [json-params]
//!
//! The saved file is a `PeerRecord` (it holds a private key; created 0600).

use serde_json::{Value, json};
use vk_e2e::PairingLink;
use vk_gateway::peer_client::{Identity, PeerClient};
use vk_gateway::state::PeerRecord;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("pair") => {
            let link = PairingLink::parse(&args[1])?;
            let file = args.get(2).cloned().unwrap_or_else(|| "device.json".into());
            let rec = PeerClient::pair_any(
                &link,
                &Identity {
                    host_name: "devclient".into(),
                    user: None,
                },
            )
            .await?;
            println!(
                "paired with {} as {} ({})",
                rec.name, rec.device_id, rec.owner
            );
            // Holds a private key: create it 0600, never through an existing file or symlink.
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&file)?
                .write_all(&serde_json::to_vec_pretty(&rec)?)?;
            println!("saved {file}");
        }
        Some("call") => {
            let rec: PeerRecord = serde_json::from_slice(&std::fs::read(&args[1])?)?;
            let params: Value = args
                .get(3)
                .map(|p| serde_json::from_str(p))
                .transpose()?
                .unwrap_or(json!({}));
            let mut c = PeerClient::connect(&rec).await?;
            eprintln!("connected: {}", c.info);
            match c.call(&args[2], params).await {
                Ok(r) => println!("{}", serde_json::to_string_pretty(&r)?),
                Err(e) => println!("error {}: {}", e.kind, e.message),
            }
        }
        Some("listen") => {
            let rec: PeerRecord = serde_json::from_slice(&std::fs::read(&args[1])?)?;
            let after: Value = args
                .get(2)
                .map(|a| serde_json::from_str(a))
                .transpose()?
                .unwrap_or(Value::Null);
            let mut c = PeerClient::connect(&rec).await?;
            let r = c
                .call_raw("events.subscribe", json!({"after": after}))
                .await
                .map_err(|e| anyhow::anyhow!("{}: {}", e.kind, e.message))?;
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
