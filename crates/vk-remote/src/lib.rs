//! Remote machines (06 Part A): the SSH stdio bridge with a channel multiplexer, the no-sudo
//! bootstrap that installs/upgrades the matching binary on the remote, and the remote side
//! (`vibeke bridge`).

pub mod bootstrap;
pub mod mux;
pub mod ssh;

pub use mux::Mux;
pub use ssh::Target;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

/// Remote side: serve the mux on stdin/stdout. Every `socket` channel connects to the local
/// server's Unix socket (spawning the server via `ensure_server` first).
pub async fn run_bridge<F, Fut>(socket: PathBuf, ensure_server: F) -> Result<()>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    let ensure = Arc::new(ensure_server);
    let acceptor: mux::Acceptor = Arc::new(move |kind: String| {
        let socket = socket.clone();
        let ensure = ensure.clone();
        Box::pin(async move {
            match kind.as_str() {
                "socket" => {
                    if tokio::net::UnixStream::connect(&socket).await.is_err() {
                        (ensure)().await?;
                    }
                    let s = tokio::net::UnixStream::connect(&socket).await?;
                    Ok(Box::new(s) as Box<dyn mux::Stream>)
                }
                other => anyhow::bail!("unknown channel kind {other}"),
            }
        })
    });
    let m = Mux::start(
        tokio::io::stdin(),
        tokio::io::stdout(),
        "bridge",
        Some(acceptor),
    );
    m.closed().await;
    Ok(())
}
