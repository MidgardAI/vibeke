//! Command line for `vibeke-relay` / `vibeke relay`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::{Config, Limits, Open, Relay, StaticTokens};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "vibeke-relay",
    version,
    about = "Relay for Vibeke gateways and apps; forwards end-to-end encrypted traffic only"
)]
struct Args {
    /// Address to listen on (put TLS in front, e.g. Caddy).
    #[arg(long, env = "VIBEKE_RELAY_LISTEN", default_value = "127.0.0.1:8787")]
    listen: SocketAddr,
    /// Public URL(s) clients use to reach this relay, e.g. https://relay.example.com (repeatable).
    #[arg(
        long = "public-url",
        env = "VIBEKE_RELAY_PUBLIC_URL",
        value_delimiter = ',',
        required = true
    )]
    public_urls: Vec<String>,
    /// Serve the web app from this directory (self-hosters only; spec 16 §9.4).
    #[arg(long, env = "VIBEKE_RELAY_APP_DIR")]
    app_dir: Option<PathBuf>,
    /// Require hosts to present one of these tokens (`?token=`); repeatable or comma-separated.
    #[arg(
        long = "host-token",
        env = "VIBEKE_RELAY_HOST_TOKENS",
        value_delimiter = ','
    )]
    host_tokens: Vec<String>,
    /// Trust X-Forwarded-For from the reverse proxy for client IPs.
    #[arg(long)]
    trust_proxy: bool,
    /// Log raw client IPs (default: daily-keyed hashes).
    #[arg(long)]
    log_ip_raw: bool,
    /// Per-connection sustained bytes/s in each direction.
    #[arg(long, default_value_t = 1024 * 1024)]
    conn_bytes_per_sec: u64,
    #[arg(long, default_value_t = 10_000)]
    max_hosts: usize,
    #[arg(long, default_value_t = 50_000)]
    max_conns: usize,
}

/// Run the CLI with `args` (without the program name). Used by the standalone binary and by
/// `vibeke relay`.
pub async fn run<I: IntoIterator<Item = String>>(args: I) -> anyhow::Result<()> {
    run_as("vibeke-relay", args).await
}

/// Like [`run`], showing `prog` (e.g. `vibeke relay`) in usage and errors.
pub async fn run_as<I: IntoIterator<Item = String>>(
    prog: &'static str,
    args: I,
) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init()
        .ok();
    use clap::{CommandFactory, FromArgMatches};
    let m = Args::command()
        .bin_name(prog)
        .name(prog)
        .get_matches_from(std::iter::once(prog.to_string()).chain(args));
    let a = Args::from_arg_matches(&m).unwrap_or_else(|e| e.exit());
    let limits = Limits {
        conn_bytes_per_sec: a.conn_bytes_per_sec,
        max_hosts: a.max_hosts,
        max_conns: a.max_conns,
        ..Limits::default()
    };
    let auth: Box<dyn crate::Authorizer> = if a.host_tokens.is_empty() {
        Box::new(Open)
    } else {
        Box::new(StaticTokens(a.host_tokens))
    };
    let relay = Relay::new(
        Config {
            public_origins: a.public_urls,
            app_dir: a.app_dir,
            trust_proxy: a.trust_proxy,
            log_ip_raw: a.log_ip_raw,
            limits,
        },
        auth,
    )?;
    let listener = tokio::net::TcpListener::bind(a.listen).await?;
    tracing::info!(listen = %a.listen, "vibeke-relay listening");
    let app = relay
        .router()
        .into_make_service_with_connect_info::<SocketAddr>();
    let drain = relay.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("draining");
            drain.drain(Duration::from_secs(30)).await;
        })
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("signal handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
}
