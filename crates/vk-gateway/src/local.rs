//! Local transport (spec 16 §9.3): the same `vibeke-e2e/1` channel over a WebSocket on a Unix
//! socket in the gateway state dir, so a desktop app on this machine skips the relay. Devices still
//! pair and authenticate with Noise exactly as over the relay; the socket only removes the hop.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use tokio::net::UnixListener;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use crate::Gateway;

pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("gateway.sock")
}

pub async fn run(gw: Arc<Gateway>) -> Result<()> {
    let path = socket_path(&gw.state.dir);
    if path.exists() {
        // Stale socket from a previous run (the state dir is 0700 and ours).
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    tracing::info!("local transport at {}", path.display());
    loop {
        let (stream, _) = listener.accept().await?;
        // Same-user peers only (as for the server socket, 09 §3.1).
        if !stream.peer_cred().is_ok_and(|c| c.uid() == uid) {
            continue;
        }
        if gw.live_connections() >= gw.limits.max_connections {
            continue;
        }
        let Some(slot) = crate::session::Dialing::try_reserve(&gw) else {
            continue;
        };
        let gw = gw.clone();
        tokio::spawn(async move {
            let cfg = WebSocketConfig::default()
                .max_message_size(Some(128 * 1024))
                .max_frame_size(Some(128 * 1024));
            let ws = match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio_tungstenite::accept_async_with_config(stream, Some(cfg)),
            )
            .await
            {
                Ok(Ok(ws)) => ws,
                _ => return,
            };
            crate::session::serve(gw, ws, slot).await;
        });
    }
}
