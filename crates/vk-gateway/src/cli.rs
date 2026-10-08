//! Command line for `vibeke-gateway` / `vibeke gateway`.

use std::path::PathBuf;
use std::time::Duration;

use crate::state::{Config, RunLock, Scope, StateDir, StatusFile};
use crate::{Gateway, pair, relay_client, server};
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

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
        /// Launched by the Vibeke server: exit 4 unless autostart is enabled for `--session`, and
        /// exit 0 when the server is gone.
        #[arg(long, hide = true)]
        autostart: bool,
    },
    /// Turn autostart on: the Vibeke server keeps the gateway running.
    On,
    /// Turn autostart off and stop the gateway the server started.
    Off,
    /// Show the supervised gateway's log.
    Logs {
        /// Keep printing new lines.
        #[arg(short, long)]
        follow: bool,
        /// Lines to show first.
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
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
        /// Relay URL to save when none is set yet (asked on a terminal otherwise).
        #[arg(long)]
        relay: Option<String>,
        /// Origin that serves the web app (see `run --app-url`). Saved.
        #[arg(long)]
        app_url: Option<String>,
        /// Self-hosting: use the relay's own origin as the app origin.
        #[arg(long, conflicts_with = "app_url")]
        app_from_relay: bool,
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
    /// List pending invitations and the share, handoff and peer devices they produced.
    Invites,
    /// Revoke a device by id or name, or cancel a pending invitation by its id.
    Revoke { device: String },
    /// Hosts this one can hand work to (spec 16 §15.3).
    Peer {
        #[command(subcommand)]
        cmd: PeerCmd,
    },
    /// Show identity and configuration.
    Status,
}

#[derive(Subcommand)]
enum PeerCmd {
    /// Print a link that pairs another of your hosts with this one (open within `--ttl`).
    Invite {
        /// Minutes the link stays valid.
        #[arg(long, default_value_t = 15)]
        ttl: u64,
    },
    /// Redeem a peer invitation (`peer.invite` on the other host) or a teammate's handoff
    /// invitation as this host.
    Add {
        link: String,
        /// Show your git user.name/email to the other host.
        #[arg(long)]
        share_user: bool,
        /// Redeem an invitation that uses a different relay than this host's (wss:// only).
        #[arg(long)]
        allow_other_relay: bool,
    },
    /// List the hosts this one can hand work to.
    List,
    /// Forget a peer by id or name.
    Remove { peer: String },
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
            autostart,
        } => {
            if autostart {
                let cfg = state.config()?;
                let want = session.clone().unwrap_or_else(|| "default".into());
                if !(cfg.autostart_enabled() && cfg.session_name() == want) {
                    eprintln!("gateway autostart is not enabled for session {want}");
                    std::process::exit(4);
                }
            }
            // Held until the process ends.
            let _run_lock = match RunLock::acquire(&state.dir)? {
                Ok(l) => l,
                Err(pid) => {
                    eprintln!("gateway already running (pid {pid})");
                    std::process::exit(3);
                }
            };
            // Launched by the server: never write gateway.toml (a concurrent `off` must win); the
            // session and socket are the supervising server's, as runtime overrides.
            let cfg = if autostart {
                state.config()?
            } else {
                let socket = socket.clone();
                let session = session.clone();
                update_config(&state, |cfg| {
                    let set_relay = relay.is_some();
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
                    for (slot, v) in [
                        (&mut cfg.relay, relay),
                        (&mut cfg.app_url, app_url),
                        (&mut cfg.session, session),
                        (&mut cfg.host_name, name),
                    ] {
                        if v.is_some() {
                            *slot = v;
                        }
                    }
                    if socket.is_some() {
                        cfg.socket = socket;
                    }
                    if set_relay {
                        cfg.autostart = Some(true);
                    }
                    Ok(())
                })?
                .1
            };
            let session_name = match (autostart, &session) {
                (true, Some(s)) => s.clone(),
                _ => cfg.session_name().to_string(),
            };
            let ppid = unsafe { libc::getppid() };
            let path = run_socket(
                autostart,
                socket,
                cfg.socket.clone(),
                std::env::var_os("VIBEKE_SOCKET").map(PathBuf::from),
                &session_name,
            );
            let gw = Gateway::new(state, server::Server::new(path))?;
            gw.status.enable(
                if gw.cfg.relay.is_some() {
                    "connecting"
                } else {
                    "local_only"
                },
                gw.cfg.relay.clone(),
                gw.devices().len(),
            );
            let r = tokio::select! {
                r = crate::run(gw.clone()) => r,
                _ = shutdown_signal() => Ok(()),
                _ = parent_gone(ppid, autostart) => Ok(()),
            };
            gw.status.clear();
            r
        }
        Cmd::On => {
            let (before, cfg) = update_config(&state, |cfg| {
                cfg.autostart = Some(true);
                Ok(())
            })?;
            let restart = needs_restart(&before, &cfg, running_status(&state.dir).as_ref());
            let st = start_gateway(&cfg, &state.dir, restart).await?;
            println!("Autostart is on.");
            print_gateway_status(&st);
            Ok(())
        }
        Cmd::Off => {
            let (_, cfg) = update_config(&state, |cfg| {
                cfg.autostart = Some(false);
                Ok(())
            })?;
            let srv =
                server::Server::new(server::socket_path(cfg.socket.clone(), cfg.session_name()));
            match srv
                .call(
                    "gateway.stop",
                    json!({"dir": state.dir.display().to_string()}),
                )
                .await
            {
                Ok(_) => println!("Autostart is off. Gateway stopped."),
                Err(e) if e.kind == "unavailable" => {
                    println!("Autostart is off. (The Vibeke server isn't running.)")
                }
                Err(e) => println!(
                    "Autostart is off. Could not stop the gateway: {}",
                    e.message
                ),
            }
            // The child needs a moment to exit; whatever still holds the lock was started by hand.
            for _ in 0..10 {
                if RunLock::holder(&state.dir).is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            if let Some(pid) = RunLock::holder(&state.dir) {
                println!(
                    "A gateway is still running (pid {pid}); it was started by hand. Stop it with Ctrl-C or `kill {pid}`."
                );
            }
            Ok(())
        }
        Cmd::Logs { follow, lines } => logs(&state.dir.join("gateway.log"), follow, lines).await,
        Cmd::Pair {
            scope,
            no_confirm,
            ttl,
            no_qr,
            local,
            relay: relay_arg,
            app_url: app_url_arg,
            app_from_relay,
        } => {
            let cfg = state.config()?;
            if local {
                let probe = crate::local::socket_path(&state.dir);
                if std::os::unix::net::UnixStream::connect(&probe).is_err() {
                    bail!(
                        "the gateway isn't running here (no {}); start it with `vibeke gateway on`",
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
            // One-command setup: save the relay, switch autostart on, start the gateway.
            let mut relay_arg = relay_arg;
            if cfg.relay.is_none() && relay_arg.is_none() {
                use std::io::IsTerminal;
                if !std::io::stdin().is_terminal() {
                    bail!(
                        "no relay set. Pass --relay <url> (and --app-url <origin> or --app-from-relay), \
                         or run `vibeke gateway pair` on a terminal"
                    );
                }
                eprint!("Relay URL: ");
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                let line = line.trim();
                if line.is_empty() {
                    bail!("no relay given");
                }
                relay_arg = Some(line.to_string());
            }
            let (before, cfg) = update_config(&state, |cfg| {
                if let Some(r) = relay_arg {
                    cfg.relay = Some(r);
                }
                if app_from_relay {
                    let Some(r) = cfg.relay.clone() else {
                        bail!("--app-from-relay needs a relay (--relay <url>)");
                    };
                    cfg.app_url = Some(relay_client::ws_base(&r).replacen("ws", "http", 1));
                } else if app_url_arg.is_some() {
                    cfg.app_url = app_url_arg;
                }
                if cfg.app_url.is_none() {
                    bail!(
                        "no app origin set. Pass --app-url <origin that serves the Vibeke web app> to `pair`, \
                         or --app-from-relay if you serve the app from your own relay (it is trusted with device keys)"
                    );
                }
                cfg.autostart = Some(true);
                Ok(())
            })?;
            // A relay that requires accounts (spec 16 §6.6): log in first, so the gateway can
            // come online and the device's link is usable.
            ensure_login(&state, &cfg).await?;
            // New connection settings reach a running gateway only through a restart.
            let restart = needs_restart(&before, &cfg, running_status(&state.dir).as_ref());
            match start_gateway(&cfg, &state.dir, restart).await {
                Ok(_) => wait_ready(&state.dir, cfg.relay.as_deref()).await,
                Err(e) => eprintln!(
                    "Could not start the gateway through the server: {e:#}\nStart it yourself with `vibeke gateway run`."
                ),
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
                owner: None,
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
                "Anyone with this link can join until it's used. List with `vibeke gateway invites`, cancel or revoke with `vibeke gateway revoke <id>`."
            );
            Ok(())
        }
        Cmd::Devices => {
            let devices = {
                let lock = state.lock()?;
                state.prune_expired_devices(&lock)?.0
            };
            if devices.is_empty() {
                println!("No paired devices. Run `vibeke-gateway pair`.");
            }
            print!("{}", format_devices(&devices, crate::state::now_s()));
            Ok(())
        }
        Cmd::Invites => {
            let devices = {
                let lock = state.lock()?;
                state.prune_expired_devices(&lock)?.0
            };
            let pending = crate::peers::pending(&state);
            let invited = crate::peers::invited_devices(&devices);
            print!(
                "{}",
                format_invites(&pending, &invited, crate::state::now_s())
            );
            Ok(())
        }
        Cmd::Revoke { device } => {
            if crate::peers::cancel_invitation(&state, &device, "cli")? {
                println!("Invitation cancelled.");
                return Ok(());
            }
            let _lock = state.lock()?;
            let mut all = state.devices()?;
            let removed: Vec<String> = all
                .iter()
                .filter(|d| d.id == device || d.name == device)
                .map(|d| d.id.clone())
                .collect();
            if removed.is_empty() {
                bail!("no device or pending invitation {device}");
            }
            all.retain(|d| !removed.contains(&d.id));
            state.save_devices(&all)?;
            for id in &removed {
                state.audit(&serde_json::json!({"ts": crate::state::now_s(), "event": "device.revoked", "device": id, "by": "cli"}));
            }
            println!("Revoked. A running gateway disconnects it within 5 s.");
            Ok(())
        }
        Cmd::Peer { cmd } => match cmd {
            PeerCmd::Invite { ttl } => {
                let cfg = state.config()?;
                let host_name = cfg
                    .host_name
                    .clone()
                    .unwrap_or_else(crate::default_host_name);
                let (p, link) = crate::peers::invite(
                    &state,
                    cfg.relay.as_deref(),
                    &host_name,
                    Duration::from_secs(ttl.clamp(1, 60) * 60),
                )?;
                state.audit(&serde_json::json!({"ts": crate::state::now_s(), "event": "share.created", "by": "cli", "kind": "peer", "pid": p.pid}));
                println!(
                    "On your other host, within {} min:\n  vibeke gateway peer add '{}'\n",
                    ttl.clamp(1, 60),
                    crate::peers::link_text(&link, cfg.app_url.as_deref())
                );
                println!("Anyone with this link can pair a host with this one until it's used.");
                Ok(())
            }
            PeerCmd::Add {
                link,
                share_user,
                allow_other_relay,
            } => {
                let cfg = state.config()?;
                start_if_enabled(&state, &cfg).await;
                let us = crate::peer_client::Identity {
                    host_name: cfg
                        .host_name
                        .clone()
                        .unwrap_or_else(crate::default_host_name),
                    user: if share_user {
                        crate::peers::git_user().await
                    } else {
                        None
                    },
                };
                let host_id = state.host_keys()?.host_id();
                let rec =
                    crate::peers::redeem(&state, &host_id, &us, &link, allow_other_relay).await?;
                let until = rec
                    .expires_at
                    .map(|t| format!(", expires {}", until_text(Some(t), crate::state::now_s())))
                    .unwrap_or_default();
                println!(
                    "Paired with {} ({}{until}). Peer id {}.",
                    rec.name,
                    if rec.owner == "self" {
                        "your host"
                    } else {
                        "teammate"
                    },
                    rec.id
                );
                Ok(())
            }
            PeerCmd::List => {
                let peers = state.peers()?;
                if peers.is_empty() {
                    println!(
                        "No peers. Run `vibeke gateway peer add <link>` with a link from `peer.invite` or a handoff invitation."
                    );
                }
                print!("{}", format_peers(&peers, crate::state::now_s()));
                Ok(())
            }
            PeerCmd::Remove { peer } => {
                let gone = crate::peers::remove(&state, &peer)?;
                println!("Removed {}.", gone.join(", "));
                Ok(())
            }
        },
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
            if cfg.relay.is_some() && cfg.relay_token.is_none() {
                println!("account      {}", crate::account::account_server(&cfg));
            }
            println!(
                "app url      {}",
                cfg.app_url
                    .as_deref()
                    .unwrap_or("(not set — pairing disabled)")
            );
            println!("devices      {}", state.devices()?.len());
            println!(
                "autostart    {}",
                if cfg.autostart_enabled() { "on" } else { "off" }
            );
            println!("running      {}", running_text(&state.dir));
            println!("state dir    {}", state.dir.display());
            Ok(())
        }
    }
}

/// SIGTERM or SIGINT.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        return std::future::pending().await;
    };
    tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
}

/// With `--autostart`: resolves once the process that launched us is gone (checked every 5 s), so a
/// crashed server never leaves an orphan. Otherwise never.
async fn parent_gone(ppid: i32, enabled: bool) {
    if !enabled {
        return std::future::pending().await;
    }
    loop {
        // SAFETY: getppid has no preconditions.
        let now = unsafe { libc::getppid() };
        if now != ppid || now == 1 {
            tracing::info!("parent process is gone; exiting");
            return;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Read-modify-write `gateway.toml` under the state dir's lock, so concurrent `on` / `off` /
/// `pair` / `run --relay` don't lose each other's change. Returns the config before and after;
/// nothing is saved when `f` fails.
fn update_config(
    state: &StateDir,
    f: impl FnOnce(&mut Config) -> Result<()>,
) -> Result<(Config, Config)> {
    let _lock = state.lock()?;
    let before = state.config()?;
    let mut cfg = before.clone();
    f(&mut cfg)?;
    state.save_config(&cfg)?;
    Ok((before, cfg))
}

/// The settings a running gateway connects with (read once at its start).
fn connection(c: &Config) -> [&Option<String>; 5] {
    [
        &c.relay,
        &c.app_url,
        &c.relay_token,
        &c.host_name,
        &c.account_url,
    ]
}

/// `pair` on a relay that requires accounts and without a stored login: run the device-code
/// login inline (same output as `vibeke login`). A private relay's static token needs none.
async fn ensure_login(state: &StateDir, cfg: &Config) -> Result<()> {
    let Some(relay) = cfg.relay.as_deref() else {
        return Ok(());
    };
    if cfg.relay_token.is_some() {
        return Ok(());
    }
    let host = state.host_keys()?.host_id();
    match crate::account::relay_auth(relay, &host).await {
        Ok(a) if a.needs_account() => {}
        Ok(_) => return Ok(()),
        Err(e) => {
            tracing::debug!("relay status: {e:#}");
            return Ok(());
        }
    }
    let acct = crate::account::account(&crate::account::account_server(cfg), &state.dir)?;
    let has = {
        let a = acct.clone();
        tokio::task::spawn_blocking(move || a.credential())
            .await??
            .is_some()
    };
    if has {
        return Ok(());
    }
    println!("This relay needs a Vibeke account. Log in first.\n");
    crate::account::login_interactive(&acct, Some(&host), true).await?;
    println!();
    Ok(())
}

/// How a gateway state reads in `status`: `login_required` spelled out with what to do.
fn state_text(state: &str) -> String {
    match state {
        "login_required" => "login required (run: vibeke login)".into(),
        s => s.into(),
    }
}

/// Whether `gateway.start` must replace a running gateway: the connection settings changed, or
/// the running one (`running`: its `status.json`) reports another relay than the saved one.
fn needs_restart(before: &Config, after: &Config, running: Option<&StatusFile>) -> bool {
    connection(before) != connection(after) || running.is_some_and(|s| s.relay != after.relay)
}

/// The `status.json` of the gateway holding `run.lock`, if one runs.
fn running_status(dir: &std::path::Path) -> Option<StatusFile> {
    let pid = RunLock::holder(dir)?;
    crate::state::read_status(dir).filter(|s| s.pid == pid)
}

/// The server socket `run` talks to. Launched by the server (`autostart`): the supervising
/// server's (`--socket`, else `$VIBEKE_SOCKET` it sets), never the saved one. By hand: `--socket`,
/// the saved socket, then the session's default.
fn run_socket(
    autostart: bool,
    arg: Option<PathBuf>,
    saved: Option<PathBuf>,
    env_socket: Option<PathBuf>,
    session: &str,
) -> PathBuf {
    if autostart {
        return arg
            .or(env_socket)
            .unwrap_or_else(|| server::socket_path(None, session));
    }
    server::socket_path(arg.or(saved), session)
}

/// Ask the session's server to start the gateway in `dir` (`restart`: replace a running one),
/// starting the server first if it isn't running (supervising `dir`).
async fn start_gateway(cfg: &Config, dir: &std::path::Path, restart: bool) -> Result<Value> {
    let srv = server::Server::new(server::socket_path(cfg.socket.clone(), cfg.session_name()));
    let params = json!({"dir": dir.display().to_string(), "restart": restart});
    match srv.call("gateway.start", params.clone()).await {
        Ok(v) => return Ok(v),
        Err(e) if e.kind == "unavailable" => {}
        Err(e) => bail!("{}: {}", e.kind, e.message),
    }
    spawn_server(cfg.session_name(), dir)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        match srv.call("gateway.start", params.clone()).await {
            Ok(v) => return Ok(v),
            Err(e) if e.kind == "unavailable" && std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => bail!("{}: {}", e.kind, e.message),
        }
    }
}

/// Start `vibeke server --session <s>` detached (own session, stdio to /dev/null), supervising
/// the gateway in `dir`.
fn spawn_server(session: &str, dir: &std::path::Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe()?;
    if exe.file_name().is_some_and(|n| n == "vibeke-gateway") {
        bail!("the Vibeke server isn't running; start it with `vibeke server`");
    }
    let mut cmd = Command::new(exe);
    cmd.args(["server", "--session", session])
        .env("VIBEKE_GATEWAY_DIR", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid between fork and exec is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}

/// `peer add`: when set up but not running, have the server start the gateway first.
async fn start_if_enabled(state: &StateDir, cfg: &Config) {
    if cfg.autostart_enabled()
        && RunLock::holder(&state.dir).is_none()
        && let Err(e) = start_gateway(cfg, &state.dir, false).await
    {
        tracing::warn!("could not start the gateway: {e:#}");
    }
}

fn print_gateway_status(st: &Value) {
    let s = |k: &str| {
        st.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("-")
            .to_string()
    };
    println!("gateway      {}", state_text(&s("state")));
    if let Some(pid) = st.get("pid").and_then(|v| v.as_u64()) {
        println!("pid          {pid}");
    }
    if let Some(e) = st.get("last_error").and_then(|v| v.as_str()) {
        println!("last error   {e}");
    }
    println!("log          {}", s("log"));
}

/// Whether the gateway is ready for pairing: `st` is the lock holder's (`holder`), and it is
/// `online` on the configured `relay` — or `local_only` when no relay is configured.
fn is_ready(st: &StatusFile, holder: Option<u32>, relay: Option<&str>) -> bool {
    if holder != Some(st.pid) {
        return false;
    }
    match relay {
        None => st.state == "local_only",
        Some(r) => st.state == "online" && st.relay.as_deref() == Some(r),
    }
}

/// Wait up to 15 s for our gateway to be ready ([`is_ready`]).
async fn wait_ready(dir: &std::path::Path, relay: Option<&str>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut shown = String::new();
    let mut last_error = None;
    println!("Starting the gateway…");
    while std::time::Instant::now() < deadline {
        let holder = RunLock::holder(dir);
        if let Some(st) = crate::state::read_status(dir)
            && holder == Some(st.pid)
        {
            if st.state != shown {
                println!("  {}", st.state);
                shown = st.state.clone();
            }
            if is_ready(&st, holder, relay) {
                return;
            }
            last_error = st.last_error;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    println!(
        "The gateway isn't online yet{}. See `vibeke gateway logs`. Continuing.",
        last_error.map(|e| format!(" ({e})")).unwrap_or_default()
    );
}

fn ago_text(now_ms: u64, since_ms: u64) -> String {
    let s = now_ms.saturating_sub(since_ms) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h {:02}m", s / 3600, s % 3600 / 60)
    } else {
        format!("{}d {}h", s / 86400, s % 86400 / 3600)
    }
}

/// The `running` line of `status`.
fn running_text(dir: &std::path::Path) -> String {
    let Some(pid) = RunLock::holder(dir) else {
        return "no".into();
    };
    match crate::state::read_status(dir).filter(|s| s.pid == pid) {
        Some(s) => {
            let mut t = format!(
                "{} · pid {pid} · for {} · {} device{}",
                state_text(&s.state),
                ago_text(crate::state::now_ms(), s.since_ms),
                s.devices,
                if s.devices == 1 { "" } else { "s" }
            );
            if let Some(e) = s.last_error.filter(|_| s.state != "login_required") {
                t.push_str(&format!(" · last error: {e}"));
            }
            t
        }
        None => format!("yes · pid {pid}"),
    }
}

/// Print the last `n` lines of `path`; with `follow`, keep printing, surviving rotation.
async fn logs(path: &std::path::Path, follow: bool, n: usize) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::fs::MetadataExt;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => Some(f),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mut offset = 0u64;
    let mut ino = 0u64;
    match file.as_mut() {
        Some(f) => {
            let mut text = Vec::new();
            f.read_to_end(&mut text)?;
            offset = text.len() as u64;
            ino = f.metadata()?.ino();
            let text = String::from_utf8_lossy(&text);
            let all: Vec<&str> = text.lines().collect();
            for l in &all[all.len().saturating_sub(n)..] {
                println!("{l}");
            }
        }
        None if !follow => {
            println!("No gateway log yet ({}).", path.display());
            return Ok(());
        }
        None => {}
    }
    if !follow {
        return Ok(());
    }
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let Ok(md) = std::fs::metadata(path) else {
            continue;
        };
        // Rotated (renamed to .1, new file) or truncated: start over on the new file.
        if md.ino() != ino || md.len() < offset {
            file = None;
            offset = 0;
            ino = md.ino();
        }
        if file.is_none() {
            file = std::fs::File::open(path).ok();
        }
        let Some(f) = file.as_mut() else { continue };
        if md.len() > offset {
            f.seek(SeekFrom::Start(offset))?;
            let mut buf = Vec::new();
            f.read_to_end(&mut buf)?;
            offset += buf.len() as u64;
            let mut out = std::io::stdout().lock();
            out.write_all(&buf)?;
            out.flush()?;
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

/// `never`, `expired` or `in 2h 05m` / `in 3d 4h` / `in 12m`.
fn until_text(t: Option<u64>, now: u64) -> String {
    let Some(t) = t else {
        return "never".into();
    };
    if t <= now {
        return "expired".into();
    }
    let s = t - now;
    let (d, h, m) = (s / 86400, s % 86400 / 3600, s % 3600 / 60);
    if d > 0 {
        format!("in {d}d {h}h")
    } else if h > 0 {
        format!("in {h}h {m:02}m")
    } else {
        format!("in {}m", m.max(1))
    }
}

fn limit_text(l: Option<&crate::state::Limit>) -> String {
    match l {
        Some(crate::state::Limit { pane: Some(p), .. }) => format!("pane {p}"),
        Some(crate::state::Limit {
            workspace: Some(w), ..
        }) => format!("workspace {w}"),
        _ => "-".into(),
    }
}

fn owner_text(d: &crate::state::Device) -> String {
    match (&d.peer, d.kind.as_str()) {
        (Some(p), _) => {
            let who = p
                .user
                .as_ref()
                .and_then(|u| u.email.clone().or_else(|| u.name.clone()));
            match who {
                Some(w) => format!("{} ({w})", p.owner),
                None => p.owner.clone(),
            }
        }
        (None, "device") => "self".into(),
        _ => "-".into(),
    }
}

/// `vibeke gateway devices`: one line per device with kind, expiry and limit.
fn format_devices(devices: &[crate::state::Device], now: u64) -> String {
    let mut out = String::new();
    if !devices.is_empty() {
        out.push_str(&format!(
            "{:<26}  {:<24} {:<8} {:<7} {:<8} {:<14} {:<20} {:<11} PUSH\n",
            "ID", "NAME", "PLATFORM", "SCOPE", "KIND", "EXPIRES", "LIMIT", "FINGERPRINT"
        ));
    }
    for d in devices {
        out.push_str(&format!(
            "{:<26}  {:<24} {:<8} {:<7} {:<8} {:<14} {:<20} {:<11} {}\n",
            d.id,
            d.name,
            d.platform,
            d.scope.as_str(),
            d.kind,
            until_text(d.expires_at, now),
            limit_text(d.limit.as_ref()),
            d.fingerprint(),
            if d.push.is_empty() { "off" } else { "on" }
        ));
    }
    out
}

/// `vibeke gateway invites`: pending invitations, then the devices invitations produced.
fn format_invites(
    pending: &[crate::state::Pairing],
    invited: &[&crate::state::Device],
    now: u64,
) -> String {
    let mut out = String::new();
    if pending.is_empty() {
        out.push_str("No pending invitations.\n");
    } else {
        out.push_str("Pending invitations (not opened yet):\n");
        out.push_str(&format!(
            "  {:<14} {:<8} {:<7} {:<14} {:<14} {:<20} LABEL\n",
            "ID", "KIND", "SCOPE", "OPEN BY", "ACCESS ENDS", "LIMIT"
        ));
        for p in pending {
            let sh = p.share.as_ref();
            let inv = crate::peers::invitation_json(p);
            let ends = inv["device_expires_at"].as_u64();
            out.push_str(&format!(
                "  {:<14} {:<8} {:<7} {:<14} {:<14} {:<20} {}\n",
                p.pid,
                sh.map_or("device", |s| s.kind.as_str()),
                p.scope.as_str(),
                until_text(Some(p.exp), now),
                until_text(ends, now),
                limit_text(sh.and_then(|s| s.limit.as_ref())),
                sh.and_then(|s| s.label.as_deref()).unwrap_or("-"),
            ));
        }
    }
    if invited.is_empty() {
        out.push_str("No share, handoff or peer devices.\n");
    } else {
        out.push_str("Devices from invitations:\n");
        out.push_str(&format!(
            "  {:<26} {:<24} {:<8} {:<7} {:<24} {:<14} LIMIT\n",
            "ID", "NAME", "KIND", "SCOPE", "OWNER", "EXPIRES"
        ));
        for d in invited {
            out.push_str(&format!(
                "  {:<26} {:<24} {:<8} {:<7} {:<24} {:<14} {}\n",
                d.id,
                d.name,
                d.kind,
                d.scope.as_str(),
                owner_text(d),
                until_text(d.expires_at, now),
                limit_text(d.limit.as_ref()),
            ));
        }
    }
    out.push_str("Cancel or revoke with `vibeke gateway revoke <id>`.\n");
    out
}

/// `vibeke gateway peer list`.
fn format_peers(peers: &[crate::state::PeerRecord], now: u64) -> String {
    let mut out = String::new();
    if !peers.is_empty() {
        out.push_str(&format!(
            "{:<26}  {:<24} {:<9} {:<14} RELAY\n",
            "ID", "NAME", "OWNER", "EXPIRES"
        ));
    }
    for p in peers {
        out.push_str(&format!(
            "{:<26}  {:<24} {:<9} {:<14} {}\n",
            p.id,
            p.name,
            p.owner,
            until_text(p.expires_at, now),
            p.relay
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Device, Limit, Pairing, PairingStatus, PeerInfo, PeerRecord, ShareSpec};

    fn device(id: &str, kind: &str, expires_at: Option<u64>, limit: Option<Limit>) -> Device {
        Device {
            id: id.into(),
            name: format!("{id}-name"),
            platform: "ios".into(),
            public: "k".into(),
            scope: Scope::View,
            paired_at: 0,
            vapid_private: None,
            push: vec![],
            prefs: Default::default(),
            push_failures: 0,
            kind: kind.into(),
            expires_at,
            limit,
            peer: None,
        }
    }

    fn status(pid: u32, state: &str, relay: Option<&str>) -> StatusFile {
        StatusFile {
            pid,
            state: state.into(),
            relay: relay.map(Into::into),
            devices: 0,
            since_ms: 0,
            last_error: None,
        }
    }

    #[test]
    fn ready_needs_the_configured_relay() {
        let r = Some("wss://new.example");
        // No relay configured: local_only is ready.
        assert!(is_ready(&status(5, "local_only", None), Some(5), None));
        // A relay configured: local_only (the old gateway) is not.
        assert!(!is_ready(&status(5, "local_only", None), Some(5), r));
        // Online, but on another relay: not ready.
        assert!(!is_ready(
            &status(5, "online", Some("wss://old.example")),
            Some(5),
            r
        ));
        assert!(!is_ready(
            &status(5, "connecting", Some("wss://new.example")),
            Some(5),
            r
        ));
        assert!(is_ready(
            &status(5, "online", Some("wss://new.example")),
            Some(5),
            r
        ));
        // Not the lock holder's file (stale, or nobody runs): never ready.
        assert!(!is_ready(
            &status(5, "online", Some("wss://new.example")),
            Some(6),
            r
        ));
        assert!(!is_ready(&status(5, "local_only", None), None, None));
    }

    #[test]
    fn restart_when_connection_settings_change() {
        let before = Config::default();
        let mut after = before.clone();
        after.autostart = Some(true);
        // Only autostart changed, nothing runs: no restart.
        assert!(!needs_restart(&before, &after, None));
        // A running local-only gateway and still no relay: no restart.
        assert!(!needs_restart(
            &before,
            &after,
            Some(&status(1, "local_only", None))
        ));
        for change in [
            |c: &mut Config| c.relay = Some("wss://r".into()),
            |c: &mut Config| c.app_url = Some("https://app".into()),
            |c: &mut Config| c.relay_token = Some("t".into()),
            |c: &mut Config| c.host_name = Some("mini".into()),
        ] {
            let mut a = after.clone();
            change(&mut a);
            assert!(needs_restart(&before, &a, None));
        }
        // Nothing changed now, but the running gateway is on another relay (`on` after an edit).
        let saved = Config {
            relay: Some("wss://new".into()),
            ..Default::default()
        };
        assert!(needs_restart(
            &saved,
            &saved,
            Some(&status(1, "online", Some("wss://old")))
        ));
        assert!(!needs_restart(
            &saved,
            &saved,
            Some(&status(1, "online", Some("wss://new")))
        ));
    }

    #[test]
    fn autostart_socket_is_the_supervisors() {
        let saved = Some(PathBuf::from("/old/vibeke.sock"));
        let env = Some(PathBuf::from("/sup/vibeke.sock"));
        // Launched by the server: its socket, never the saved one.
        assert_eq!(
            run_socket(true, None, saved.clone(), env.clone(), "default"),
            PathBuf::from("/sup/vibeke.sock")
        );
        assert_eq!(
            run_socket(
                true,
                Some("/arg.sock".into()),
                saved.clone(),
                env.clone(),
                "default"
            ),
            PathBuf::from("/arg.sock")
        );
        // By hand: --socket, then the saved one.
        assert_eq!(
            run_socket(false, None, saved.clone(), env.clone(), "default"),
            PathBuf::from("/old/vibeke.sock")
        );
        assert_eq!(
            run_socket(false, Some("/arg.sock".into()), saved, env, "default"),
            PathBuf::from("/arg.sock")
        );
    }

    #[test]
    fn config_updates_are_read_modify_write() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::open(dir.path().join("gw")).unwrap();
        let (before, after) = update_config(&state, |c| {
            c.relay = Some("wss://r".into());
            Ok(())
        })
        .unwrap();
        assert!(before.relay.is_none() && after.relay.is_some());
        // A later update starts from what is saved now, not from an older read.
        let (before, after) = update_config(&state, |c| {
            c.autostart = Some(false);
            Ok(())
        })
        .unwrap();
        assert_eq!(before.relay.as_deref(), Some("wss://r"));
        assert_eq!(after.autostart, Some(false));
        assert_eq!(state.config().unwrap().relay.as_deref(), Some("wss://r"));
        // A failing update saves nothing.
        assert!(
            update_config(&state, |c| {
                c.relay = None;
                bail!("no")
            })
            .is_err()
        );
        assert_eq!(state.config().unwrap().relay.as_deref(), Some("wss://r"));
    }

    #[test]
    fn ago_text_units() {
        assert_eq!(ago_text(10_000, 4_000), "6s");
        assert_eq!(ago_text(400_000, 0), "6m");
        assert_eq!(ago_text(3_900_000, 0), "1h 05m");
    }

    #[test]
    fn durations_and_expiry_text() {
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert_eq!(until_text(None, 100), "never");
        assert_eq!(until_text(Some(50), 100), "expired");
        assert_eq!(until_text(Some(100 + 2 * 3600 + 5 * 60), 100), "in 2h 05m");
        assert_eq!(
            until_text(Some(100 + 3 * 86400 + 4 * 3600), 100),
            "in 3d 4h"
        );
        assert_eq!(until_text(Some(130), 100), "in 1m");
    }

    #[test]
    fn devices_show_kind_expiry_and_limit() {
        let now = 1_000_000;
        let out = format_devices(
            &[
                device("d1", "device", None, None),
                device(
                    "s1",
                    "share",
                    Some(now + 7200),
                    Some(Limit {
                        workspace: Some("w1".into()),
                        pane: None,
                    }),
                ),
            ],
            now,
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].contains("KIND") && lines[0].contains("EXPIRES") && lines[0].contains("LIMIT")
        );
        assert!(lines[1].contains("device") && lines[1].contains("never"));
        assert!(
            lines[2].contains("share")
                && lines[2].contains("in 2h 00m")
                && lines[2].contains("workspace w1")
        );
    }

    #[test]
    fn invites_list_pending_and_devices() {
        let now = 1_000_000;
        let pending = vec![Pairing {
            pid: "abcdefghijkl".into(),
            psk: "x".into(),
            exp: now + 600,
            scope: Scope::Full,
            name: None,
            no_confirm: true,
            status: PairingStatus::Pending,
            confirmed: None,
            confirmed_claim: None,
            share: Some(ShareSpec {
                kind: "handoff".into(),
                ttl_s: 86400,
                until: now + 86400,
                limit: None,
                label: Some("for Kari".into()),
                owner: None,
            }),
            created_at: now,
        }];
        let mut peer = device("p1", "peer", None, None);
        peer.peer = Some(PeerInfo {
            owner: "self".into(),
            host_name: Some("laptop".into()),
            user: None,
        });
        let mut handoff = device("h1", "peer", Some(now + 3600), None);
        handoff.peer = Some(PeerInfo {
            owner: "teammate".into(),
            host_name: None,
            user: None,
        });
        let out = format_invites(&pending, &[&peer, &handoff], now);
        assert!(
            out.contains("abcdefghijkl") && out.contains("handoff") && out.contains("for Kari")
        );
        assert!(out.contains("in 10m") && out.contains("in 1d 0h"), "{out}");
        let peer_line = out.lines().find(|l| l.contains("p1-name")).unwrap();
        assert!(
            peer_line.contains("peer") && peer_line.contains("self") && peer_line.contains("never")
        );
        let h_line = out.lines().find(|l| l.contains("h1-name")).unwrap();
        assert!(h_line.contains("teammate") && h_line.contains("in 1h 00m"));
        let empty = format_invites(&[], &[], now);
        assert!(
            empty.contains("No pending invitations")
                && empty.contains("No share, handoff or peer devices")
        );
    }

    #[test]
    fn peers_list() {
        let out = format_peers(
            &[PeerRecord {
                id: "x".into(),
                name: "devbox".into(),
                relay: "wss://relay".into(),
                host: "h".into(),
                host_key: "hk".into(),
                device_key: "secret".into(),
                device_id: "d".into(),
                owner: "teammate".into(),
                added_at: 0,
                expires_at: None,
                ticket: None,
                ticket_exp: None,
            }],
            0,
        );
        assert!(out.contains("devbox") && out.contains("teammate") && out.contains("wss://relay"));
        assert!(!out.contains("secret"));
    }
}
