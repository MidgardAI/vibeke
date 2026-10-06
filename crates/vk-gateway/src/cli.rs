//! Command line for `vibeke-gateway` / `vibeke gateway`.

use std::path::PathBuf;
use std::time::Duration;

use crate::state::{Scope, StateDir};
use crate::{Gateway, pair, relay_client, server};
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

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
        /// For an app on this machine (the desktop app): a link to the local socket, printed as
        /// JSON; no confirmation (same user as this command).
        #[arg(long)]
        local: bool,
    },
    /// Share a pane or workspace with someone: an expiring, scoped invitation link (spec 16 §15.1).
    /// With --handoff, an invitation that lets a teammate hand work to this host instead.
    Share {
        #[arg(long, default_value = "view", value_parser = ["view", "approve"])]
        scope: String,
        /// Duration, e.g. 30m, 2h, 1d.
        #[arg(long, default_value = "2h")]
        ttl: String,
        #[arg(long, conflicts_with = "pane")]
        workspace: Option<String>,
        #[arg(long)]
        pane: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long, conflicts_with_all = ["workspace", "pane", "scope"])]
        handoff: bool,
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

/// Run the CLI with `args` (without the program name). Used by the standalone binary and by
/// `vibeke gateway`.
pub async fn run<I: IntoIterator<Item = String>>(args: I) -> Result<()> {
    run_as("vibeke-gateway", args).await
}

/// Like [`run`], showing `prog` (e.g. `vibeke gateway`) in usage and errors.
pub async fn run_as<I: IntoIterator<Item = String>>(prog: &'static str, args: I) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .try_init()
        .ok();
    use clap::{CommandFactory, FromArgMatches};
    let m = Args::command()
        .bin_name(prog)
        .name(prog)
        .get_matches_from(std::iter::once(prog.to_string()).chain(args));
    let a = Args::from_arg_matches(&m).unwrap_or_else(|e| e.exit());
    let state = StateDir::open(a.dir.unwrap_or_else(crate::state::default_dir))?;
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
            crate::run(gw).await
        }
        Cmd::Pair {
            scope,
            no_confirm,
            ttl,
            no_qr,
            local,
        } => {
            let cfg = state.config()?;
            if local {
                let probe = crate::local::socket_path(&state.dir);
                if std::os::unix::net::UnixStream::connect(&probe).is_err() {
                    bail!(
                        "the gateway isn't running here (no {}); start it with `vibeke gateway run`",
                        probe.display()
                    );
                }
                let host_name = cfg
                    .host_name
                    .clone()
                    .unwrap_or_else(crate::default_host_name);
                let sock = crate::local::socket_path(&state.dir);
                let (p, mut link) = pair::create(
                    &state,
                    "local",
                    &host_name,
                    scope,
                    true,
                    Duration::from_secs(ttl * 60),
                )?;
                link.relay = format!("local:{}", sock.display());
                let out = serde_json::json!({"link": link, "d": vk_e2e::b64::encode(serde_json::to_vec(&link)?), "pid": p.pid, "socket": sock});
                println!("{out}");
                return Ok(());
            }
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
        Cmd::Share {
            scope,
            ttl,
            workspace,
            pane,
            label,
            handoff,
            no_qr,
        } => {
            let cfg = state.config()?;
            let Some(relay) = cfg.relay.clone() else {
                bail!("run `vibeke gateway run --relay <url>` once first")
            };
            let Some(app) = cfg.app_url.clone() else {
                bail!("no app origin set (vibeke gateway run --app-url …)")
            };
            let ttl_s = parse_duration(&ttl)?;
            let limit = if handoff {
                None
            } else {
                // Resolve handles (w1, w1:p2) to ids through the server.
                let srv = server::Server::new(server::socket_path(
                    cfg.socket.clone(),
                    cfg.session.as_deref().unwrap_or("default"),
                ));
                let call = |m: &'static str, p: serde_json::Value| {
                    let srv = srv.clone();
                    async move {
                        srv.call(m, p)
                            .await
                            .map_err(|e| anyhow::anyhow!("{}: {}", e.kind, e.message))
                    }
                };
                match (workspace, pane) {
                    (_, Some(p)) => {
                        let r = call("pane.get", serde_json::json!({"pane": p})).await?;
                        let id = r
                            .pointer("/pane/id")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&p)
                            .to_string();
                        Some(crate::state::Limit {
                            workspace: None,
                            pane: Some(id),
                        })
                    }
                    (Some(w), None) => {
                        let r = call("workspace.get", serde_json::json!({"workspace": w})).await?;
                        let id = r
                            .pointer("/workspace/id")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&w)
                            .to_string();
                        Some(crate::state::Limit {
                            workspace: Some(id),
                            pane: None,
                        })
                    }
                    (None, None) => bail!("share --workspace W or --pane P (or --handoff)"),
                }
            };
            let scope: Scope = if handoff { Scope::Full } else { scope.parse()? };
            let kind = if handoff { "handoff" } else { "share" };
            let host_name = cfg.host_name.clone().unwrap_or_else(|| "this host".into());
            let until = crate::state::now_s() + ttl_s;
            let spec = crate::state::ShareSpec {
                kind: kind.into(),
                ttl_s,
                until,
                limit,
                label,
            };
            let (_, link) = pair::create_with(
                &state,
                &relay,
                &host_name,
                scope,
                true,
                Duration::from_secs(15 * 60),
                Some(spec),
            )?;
            let url = link.to_url(&app);
            if !no_qr {
                println!("{}", pair::render_qr(&url));
            }
            let what = if handoff {
                "Handoff invitation".to_string()
            } else {
                format!("Share ({})", scope.as_str())
            };
            println!("{what}, open within 15 min, access for {ttl}:\n{url}\n");
            println!(
                "Anyone with this link can join until it's used. Revoke later with `vibeke gateway revoke <device>`."
            );
            Ok(())
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

/// `30m`, `2h`, `1d`, `90s` or plain seconds.
fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n.parse().map_err(|_| anyhow::anyhow!("bad duration {s}"))?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => bail!("bad duration {s} (use 30m, 2h, 1d)"),
    };
    Ok(secs.clamp(60, 7 * 86400))
}
