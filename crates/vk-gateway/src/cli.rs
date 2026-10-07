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
            PeerCmd::Add { link, share_user } => {
                let cfg = state.config()?;
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
                let rec = crate::peers::redeem(&state, &host_id, &us, &link).await?;
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
        (None, "handoff") => "teammate".into(),
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
        let handoff = device("h1", "handoff", Some(now + 3600), None);
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
            }],
            0,
        );
        assert!(out.contains("devbox") && out.contains("teammate") && out.contains("wss://relay"));
        assert!(!out.contains("secret"));
    }
}
