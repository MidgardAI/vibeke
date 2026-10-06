//! `vibeke-gateway` (spec 16 §7.1).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use vk_gateway::state::{Scope, StateDir};
use vk_gateway::{Gateway, pair, relay_client, server};

#[derive(Parser)]
#[command(
    name = "vibeke-gateway",
    version,
    about = "Reach this host's Vibeke agents from your phone through an end-to-end encrypted relay"
)]
struct Args {
    /// State directory (keys, devices, pairings). Default: <config>/vibeke/gateway.
    #[arg(long, global = true, env = "VIBEKE_GATEWAY_DIR")]
    dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the gateway in the foreground.
    Run {
        /// Relay URL, e.g. https://relay.example.com (saved to gateway.toml).
        #[arg(long)]
        relay: Option<String>,
        /// Origin that serves the web app; pairing links point here, and whoever serves it is
        /// trusted with device keys (spec 16 §9.4). Saved.
        #[arg(long)]
        app_url: Option<String>,
        /// Self-hosting: you serve the app from your own relay (`--app-dir`); use its origin.
        #[arg(long, conflicts_with = "app_url")]
        app_from_relay: bool,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        socket: Option<PathBuf>,
        /// Display name for this host (saved).
        #[arg(long)]
        name: Option<String>,
    },
    /// Pair a phone or browser: shows a QR code, then asks you to confirm the device.
    Pair {
        #[arg(long, default_value = "full")]
        scope: Scope,
        /// Skip the fingerprint confirmation: the QR becomes a bearer invitation.
        #[arg(long)]
        no_confirm: bool,
        /// Minutes the code stays valid.
        #[arg(long, default_value_t = 10)]
        ttl: u64,
        /// Print the link only (no QR).
        #[arg(long)]
        no_qr: bool,
    },
    /// List paired devices.
    Devices,
    /// Revoke a device by id or name.
    Revoke { device: String },
    /// Show identity and configuration.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let a = Args::parse();
    let state = StateDir::open(a.dir.unwrap_or_else(vk_gateway::state::default_dir))?;
    match a.cmd {
        Cmd::Run {
            relay,
            app_url,
            app_from_relay,
            session,
            socket,
            name,
        } => {
            let mut cfg = state.config()?;
            let app_url = match (app_from_relay, app_url) {
                (true, _) => {
                    let r = relay
                        .clone()
                        .or(cfg.relay.clone())
                        .ok_or_else(|| anyhow::anyhow!("--app-from-relay needs --relay"))?;
                    Some(relay_client::ws_base(&r).replacen("ws", "http", 1))
                }
                (false, u) => u,
            };
            let mut changed = false;
            for (slot, v) in [
                (&mut cfg.relay, relay),
                (&mut cfg.app_url, app_url),
                (&mut cfg.session, session),
                (&mut cfg.host_name, name),
            ] {
                if v.is_some() && *slot != v {
                    *slot = v;
                    changed = true;
                }
            }
            if socket.is_some() && cfg.socket != socket {
                cfg.socket = socket;
                changed = true;
            }
            if changed {
                state.save_config(&cfg)?;
            }
            let path = server::socket_path(
                cfg.socket.clone(),
                cfg.session.as_deref().unwrap_or("default"),
            );
            let gw = Gateway::new(state, server::Server::new(path))?;
            vk_gateway::run(gw).await
        }
        Cmd::Pair {
            scope,
            no_confirm,
            ttl,
            no_qr,
        } => {
            let cfg = state.config()?;
            let Some(relay) = cfg.relay.clone() else {
                bail!("run `vibeke-gateway run --relay <url>` once first")
            };
            let host_name = cfg.host_name.clone().unwrap_or_else(|| "this host".into());
            let (p, link) = pair::create(
                &state,
                &relay,
                &host_name,
                scope,
                no_confirm,
                Duration::from_secs(ttl * 60),
            )?;
            let Some(app) = cfg.app_url.clone() else {
                bail!(
                    "no app origin set. Pass --app-url <origin that serves the Vibeke web app> to `run`, \
                     or --app-from-relay if you serve the app from your own relay (it is trusted with device keys)"
                );
            };
            let url = link.to_url(&app);
            if !no_qr {
                println!("{}", pair::render_qr(&url));
            }
            println!(
                "Open on your phone (valid {ttl} min, single use, scope {}):\n{url}\n",
                scope.as_str()
            );
            if no_confirm {
                println!("Bearer invitation: whoever opens this link first gets access.");
            } else {
                println!(
                    "Waiting for the device… keep this running; you'll be asked to confirm its fingerprint."
                );
            }
            pair::wait_and_confirm(&state, &p.pid, p.exp).await
        }
        Cmd::Devices => {
            let devices = state.devices()?;
            if devices.is_empty() {
                println!("No paired devices. Run `vibeke-gateway pair`.");
            }
            for d in devices {
                println!(
                    "{}  {:<24} {:<8} {:<7} fp {}  push {}",
                    d.id,
                    d.name,
                    d.platform,
                    d.scope.as_str(),
                    d.fingerprint(),
                    if d.push.is_empty() { "off" } else { "on" }
                );
            }
            Ok(())
        }
        Cmd::Revoke { device } => {
            let _lock = state.lock()?;
            let mut all = state.devices()?;
            let n = all.len();
            all.retain(|d| d.id != device && d.name != device);
            if all.len() == n {
                bail!("no device {device}");
            }
            state.save_devices(&all)?;
            println!("Revoked. A running gateway disconnects it within 5 s.");
            Ok(())
        }
        Cmd::Status => {
            let keys = state.host_keys()?;
            let cfg = state.config()?;
            println!("host id      {}", keys.host_id());
            println!(
                "fingerprint  {}",
                vk_e2e::keys::fingerprint(&keys.noise_public())
            );
            println!(
                "relay        {}",
                cfg.relay.as_deref().unwrap_or("(not set)")
            );
            println!(
                "app url      {}",
                cfg.app_url
                    .as_deref()
                    .unwrap_or("(not set — pairing disabled)")
            );
            println!("devices      {}", state.devices()?.len());
            println!("state dir    {}", state.dir.display());
            Ok(())
        }
    }
}
