//! Remote machines over SSH (06 Part A): `vibeke ssh <host>`, `vibeke bridge` (remote side),
//! saved machines (`vibeke machine …`), and `--machine` forwarding (never falls back to local).

use serde_json::{Value, json};
use std::path::PathBuf;
use vk_cli::{EXIT_API, EXIT_OK, EXIT_USAGE, Global};
use vk_remote::bootstrap::{self, Artifact};
use vk_remote::{Class, Link, LinkStatus, Target};

/// Remote side of the link: stdio mux → this machine's server socket.
pub async fn bridge(g: &Global, _args: &[String]) -> i32 {
    let socket = vk_cli::client::socket_path(&g.session, None);
    let session = g.session.clone();
    let opts = vk_remote::BridgeOpts {
        allow_egress: allow_remote_egress(&crate::commands::load_config()),
    };
    let r = vk_remote::run_bridge(
        socket.clone(),
        move || {
            let session = session.clone();
            let socket = socket.clone();
            async move {
                vk_cli::client::connect_or_spawn(&session, &socket, false).await?;
                Ok(())
            }
        },
        opts,
    )
    .await;
    match r {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("bridge: {e:#}");
            EXIT_API
        }
    }
}

/// In-box end of a container box's link (13 §4/§7, `vk_remote::boxlink`):
/// `vibeke sandbox bridge [--listen 127.0.0.1:3128] [--brokers DIR]` over stdin/stdout.
pub async fn box_bridge(args: &[String]) -> i32 {
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let tcp = match get("--listen") {
        Some(addr) => match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("vibeke sandbox bridge: listen {addr}: {e}");
                return EXIT_API;
            }
        },
        None => None,
    };
    let dir = get("--brokers")
        .map(PathBuf::from)
        .unwrap_or_else(|| vk_remote::boxlink::default_broker_dir().to_path_buf());
    match vk_remote::boxlink::box_side(tokio::io::stdin(), tokio::io::stdout(), tcp, dir).await {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("vibeke sandbox bridge: {e:#}");
            EXIT_API
        }
    }
}

/// `[preview] allow_remote_egress` on this (remote) machine; default true (06 B3.4).
fn allow_remote_egress(cfg: &vk_config::Config) -> bool {
    cfg.preview().allow_remote_egress
}

pub fn target_for(cfg: &vk_config::Config, host: &str) -> Target {
    match cfg.remote.machine.iter().find(|m| m.label == host) {
        Some(m) => Target::parse(&m.label, &m.address),
        None => {
            let h = host.rsplit('@').next().unwrap_or(host);
            let label = if h.parse::<std::net::IpAddr>().is_ok() {
                h.to_string()
            } else {
                h.split('.').next().unwrap_or(h).to_string()
            };
            Target::parse(&label, host)
        }
    }
}

/// Locally available artifact for a remote target (`linux-x86_64`, `linux-aarch64`, …):
/// `$VIBEKE_ARTIFACT_DIR` or `~/.cache/vibeke/releases/<version>/vibeke-<target>` with a
/// `.sha256` sidecar or `SHA256SUMS`. An artifact is returned only if its checksum comes from
/// such a file and it is signed or `VIBEKE_ALLOW_UNSIGNED=1` is set; the running binary itself
/// (when the remote matches this platform) is offered only with that opt-in.
pub fn artifact_for(target: &str) -> Option<Artifact> {
    artifact_for_with(target, bootstrap::allow_unsigned_env())
}

pub use vk_remote::link::check_session;

fn artifact_for_with(target: &str, allow_unsigned: bool) -> Option<Artifact> {
    let version = vk_proto::VERSION.to_string();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::var_os("VIBEKE_ARTIFACT_DIR") {
        dirs.push(PathBuf::from(d));
    }
    dirs.push(
        vk_server::paths::home()
            .join(".cache/vibeke/releases")
            .join(&version),
    );
    for d in dirs {
        let p = d.join(format!("vibeke-{target}"));
        if p.exists() {
            // The expected checksum comes from a file next to the artifact, never from the
            // artifact itself; a candidate that fails the trust check is skipped.
            match bootstrap::load_artifact(p.clone(), version.clone(), allow_unsigned) {
                Ok(a) => return Some(a),
                Err(e) => {
                    tracing::warn!("ignoring artifact {}: {e:#}", p.display());
                    continue;
                }
            }
        }
    }
    let here = format!(
        "{}-{}",
        if cfg!(target_os = "macos") {
            "macos"
        } else {
            "linux"
        },
        std::env::consts::ARCH
    );
    if allow_unsigned && here == target && cfg!(target_env = "musl") {
        let exe = std::env::current_exe().ok()?;
        return bootstrap::load_self_artifact(exe, version, true).ok();
    }
    None
}

/// The TUI's view of a link: render stream and uploads on their own scheduling classes
/// (06 A4), and the link state for the sidebar and frame pacing (06 A7).
fn spec_for(link: Link) -> vk_tui::app::MachineSpec {
    let label = link.label().to_string();
    let (render, bulk, probe) = (link.clone(), link.clone(), link);
    vk_tui::app::MachineSpec {
        label,
        local: false,
        connect: Box::new(move || {
            let link = render.clone();
            Box::pin(async move {
                let s = link.open_class(Class::Render).await?;
                Ok(Box::new(s) as vk_tui::app::Stream)
            })
        }),
        bulk: Some(Box::new(move || {
            let link = bulk.clone();
            Box::pin(async move {
                let s = link.open_class(Class::Blob).await?;
                Ok(Box::new(s) as vk_tui::app::Stream)
            })
        })),
        link: Some(std::sync::Arc::new(move || link_info(&probe.status()))),
    }
}

fn link_info(s: &LinkStatus) -> vk_tui::remote_view::LinkInfo {
    vk_tui::remote_view::LinkInfo {
        state: s.state.as_str().into(),
        rtt_ms: s.rtt_ms,
        last_seen_ms: s.last_seen_ms,
    }
}

/// Saved machines with `auto_connect` for the unified multi-machine view (06 A5).
pub fn specs(cfg: &vk_config::Config, g: &Global) -> Vec<vk_tui::app::MachineSpec> {
    cfg.remote
        .machine
        .iter()
        .filter(|m| m.auto_connect)
        .map(|m| spec_for(Link::new(Target::parse(&m.label, &m.address), &g.session)))
        .collect()
}

/// `vibeke ssh <host> [--upgrade] [--yes] [--allow-downgrade] [--remote-session s]`: probe,
/// install/upgrade the remote binary (no sudo), then attach the TUI to the remote server over
/// the bridge. A remote newer than this release is never downgraded without
/// `--allow-downgrade`.
pub async fn ssh(g: &Global, args: &[String]) -> i32 {
    let Some(host) = args.iter().find(|a| !a.starts_with("--")) else {
        eprintln!("vibeke ssh <host|label> [--upgrade] [--allow-downgrade] [--no-local]");
        return EXIT_USAGE;
    };
    if let Err(e) = check_session(&g.session) {
        eprintln!("vibeke: {e:#}");
        return EXIT_USAGE;
    }
    let upgrade = args.iter().any(|a| a == "--upgrade" || a == "--yes");
    let allow_downgrade = args.iter().any(|a| a == "--allow-downgrade");
    let cfg = crate::commands::load_config();
    let target = target_for(&cfg, host);
    let auto_upgrade = cfg
        .remote
        .machine
        .iter()
        .find(|m| m.label == target.label)
        .is_some_and(|m| m.auto_upgrade);
    let remote_download = cfg
        .remote
        .machine
        .iter()
        .find(|m| m.label == target.label)
        .is_some_and(|m| m.bootstrap == vk_config::Bootstrap::RemoteDownload);
    eprintln!("vibeke: probing {} …", target.address);
    let probe = match bootstrap::probe(&target).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e:#}");
            return EXIT_API;
        }
    };
    let artifact = if remote_download {
        None
    } else {
        artifact_for(&probe.target())
    };
    if let Some(a) = &artifact
        && a.trust != bootstrap::Trust::Signed
    {
        eprintln!(
            "vibeke: {}",
            bootstrap::unsigned_warning(&a.path, &a.sha256)
        );
    }
    let ensured = if remote_download {
        ensure_via_download(&target, &probe, upgrade || auto_upgrade, allow_downgrade).await
    } else {
        bootstrap::ensure(
            &target,
            &probe,
            artifact.as_ref(),
            upgrade || auto_upgrade,
            allow_downgrade,
        )
        .await
    };
    match ensured {
        Ok(bootstrap::Outcome::AlreadyCurrent) => {}
        Ok(o) => {
            eprintln!(
                "vibeke: {:?} vibeke {} on {} ({})",
                o,
                vk_proto::VERSION,
                target.label,
                probe.target()
            );
            if o == bootstrap::Outcome::Upgraded {
                // Restart the remote server on the new binary; holders keep every pane alive.
                let _ = target
                    .run(
                        &format!(
                            "{} server restart --session {}",
                            vk_remote::ssh::sh_quote(bootstrap::REMOTE_BIN),
                            vk_remote::ssh::sh_quote(&g.session)
                        ),
                        None,
                    )
                    .await;
            }
        }
        Err(e) => {
            eprintln!("vibeke: {e:#}");
            return EXIT_API;
        }
    }
    let link = Link::new(target.clone(), &g.session);
    if let Err(e) = link.open().await {
        eprintln!("vibeke: bridge to {}: {e:#}", target.label);
        return EXIT_API;
    }
    let mut specs = Vec::new();
    if !args.iter().any(|a| a == "--no-local") && std::env::var("VIBEKE").is_err() {
        let socket = vk_cli::client::socket_path(&g.session, None);
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            specs.push(crate::commands::local_spec(&g.session, socket));
        }
    }
    let remote_index = specs.len();
    specs.push(spec_for(link));
    let opts = vk_tui::app::Opts {
        session: g.session.clone(),
        config: cfg,
        initial_machine: remote_index,
    };
    match vk_tui::app::run(opts, specs).await {
        Ok(r) => {
            eprintln!("[{r}]");
            EXIT_OK
        }
        Err(e) => {
            eprintln!("vibeke: {e:#}");
            EXIT_API
        }
    }
}

/// Connection for `--machine` CLI forwarding: a channel to the remote server; no local fallback.
pub async fn machine_stream(g: &Global, machine: &str) -> anyhow::Result<tokio::io::DuplexStream> {
    check_session(&g.session)?;
    let cfg = crate::commands::load_config();
    let target = target_for(&cfg, machine);
    let link = Link::new(target, &g.session);
    let s = link.open().await?;
    // The CLI exits after one command; keep the ssh link alive until then.
    std::mem::forget(link);
    Ok(s)
}

// ---- machine show / connect / disconnect / upgrade (06 A2) -------------------------------

fn flag(args: &[String], f: &str) -> bool {
    args.iter().any(|a| a == f)
}

fn opt(args: &[String], k: &str) -> Option<String> {
    args.iter()
        .position(|a| a == k)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// The first positional argument (skipping flags and their values).
fn positional<'a>(args: &'a [String], valued: &[&str]) -> Option<&'a String> {
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if valued.contains(&a.as_str()) {
            skip = true;
            continue;
        }
        if !a.starts_with("--") {
            return Some(a);
        }
    }
    None
}

fn want_json(g: &Global, args: &[String]) -> bool {
    flag(args, "--json") || g.json == Some(true)
}

fn print_out(json_out: bool, v: &Value, text: String) {
    if json_out {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    } else {
        print!("{text}");
    }
}

fn unavailable(machine: &str, e: impl std::fmt::Display) -> i32 {
    eprintln!(
        "{}",
        json!({"error": {"kind": "remote_unavailable", "message": e.to_string(), "details": {"machine": machine}, "retryable": true}})
    );
    EXIT_API
}

/// `vibeke machine <verb>`: the link verbs are async; list/add/remove/status/doctor edit or
/// probe the saved machines.
pub async fn machine(g: &Global, args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("show") => machine_show(g, &args[1..]).await,
        Some("connect") => machine_connect(g, &args[1..]).await,
        Some("disconnect") => machine_disconnect(g, &args[1..]).await,
        Some("upgrade") => machine_upgrade(g, &args[1..]).await,
        _ => machine_cmd(g, args),
    }
}

/// Open the bridge and ask the remote server for its status (spawning it if needed).
async fn connect_link(g: &Global, target: Target) -> (Link, Result<Value, String>) {
    let link = Link::new(target, &g.session);
    let r = async {
        check_session(&g.session)?;
        let s = link.open().await?;
        let mut c = vk_cli::client::Client::new(s);
        c.call("server.status", json!({}))
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
    .await;
    (link, r.map_err(|e| format!("{e:#}")))
}

fn link_json(s: &LinkStatus) -> Value {
    json!({
        "state": s.state.as_str(),
        "rtt_ms": s.rtt_ms,
        "last_seen_ms": s.last_seen_ms,
        "bytes_in": s.bytes_in,
        "bytes_out": s.bytes_out,
        "payload_out": s.payload_out,
        "zstd_frames_out": s.zstd_frames_out,
        "remote_version": s.remote_version,
    })
}

fn trust_name(t: bootstrap::Trust) -> &'static str {
    match t {
        bootstrap::Trust::Signed => "signed",
        bootstrap::Trust::UnsignedOptIn => "unsigned (VIBEKE_ALLOW_UNSIGNED=1)",
        bootstrap::Trust::SelfHashedOptIn => "unsigned local build (VIBEKE_ALLOW_UNSIGNED=1)",
    }
}

/// `vibeke machine show <m> [--offline] [--json]`: saved settings, then (unless `--offline`)
/// the remote's platform and installed version, the live link (state, RTT, compression), the
/// remote server, and whether a verified local artifact for its platform exists (09 §7: an
/// unsigned build is marked as such).
async fn machine_show(g: &Global, args: &[String]) -> i32 {
    let Some(label) = positional(args, &[]) else {
        eprintln!("vibeke machine show <machine> [--offline] [--json]");
        return EXIT_USAGE;
    };
    let cfg = crate::commands::load_config();
    let target = target_for(&cfg, label);
    let saved = cfg
        .remote
        .machine
        .iter()
        .find(|m| &m.label == label)
        .and_then(|m| serde_json::to_value(m).ok());
    let mut out = json!({
        "machine": target.label,
        "address": target.address,
        "port": target.port,
        "saved": saved,
    });
    let mut text = format!("{}  {}\n", target.label, target.address);
    if let Some(s) = &out["saved"].as_object() {
        text.push_str(&format!(
            "  saved        auto_connect={} auto_upgrade={} bootstrap={} transport={}\n",
            s["auto_connect"], s["auto_upgrade"], s["bootstrap"], s["transport"]
        ));
    } else {
        text.push_str("  saved        no (ad-hoc address)\n");
    }
    if !flag(args, "--offline") {
        match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            bootstrap::probe(&target),
        )
        .await
        {
            Ok(Ok(p)) => {
                let art = artifact_for(&p.target());
                out["remote"] = json!({"os": p.os, "arch": p.arch, "libc": p.libc, "home": p.home,
                                       "target": p.target(), "vibeke": p.version});
                out["local_artifact"] = match &art {
                    Some(a) => {
                        json!({"path": a.path, "version": a.version, "trust": trust_name(a.trust)})
                    }
                    None => Value::Null,
                };
                text.push_str(&format!(
                    "  remote       {} {} {}, vibeke {}\n",
                    p.os,
                    p.arch,
                    p.libc,
                    p.version.as_deref().unwrap_or("not installed")
                ));
                text.push_str(&format!(
                    "  local build  {}\n",
                    match &art {
                        Some(a) => format!(
                            "vibeke {} for {}: {}",
                            a.version,
                            p.target(),
                            trust_name(a.trust)
                        ),
                        None => format!("none verified for {}", p.target()),
                    }
                ));
                let (link, st) = connect_link(g, target.clone()).await;
                let ls = link.status();
                out["link"] = link_json(&ls);
                match st {
                    Ok(v) => {
                        out["server"] = json!({"version": v["version"], "panes": v["panes"],
                                               "event_seq": v["event_seq"]});
                        text.push_str(&format!(
                            "  link         {}{}{}\n",
                            ls.state.as_str(),
                            ls.rtt_ms
                                .map(|r| format!(", rtt {r} ms"))
                                .unwrap_or_default(),
                            if ls.zstd_frames_out > 0 {
                                ", zstd on"
                            } else {
                                ""
                            }
                        ));
                        text.push_str(&format!(
                            "  server       vibeke {}, {} pane(s)\n",
                            v["version"].as_str().unwrap_or("?"),
                            v["panes"]
                        ));
                    }
                    Err(e) => {
                        out["server"] = json!({"error": e});
                        text.push_str(&format!("  link         {} ({e})\n", ls.state.as_str()));
                    }
                }
                link.disconnect().await;
            }
            Ok(Err(e)) => {
                out["remote"] = json!({"error": format!("{e:#}")});
                text.push_str(&format!("  remote       unreachable: {e:#}\n"));
            }
            Err(_) => {
                out["remote"] = json!({"error": "probe timed out"});
                text.push_str("  remote       unreachable: probe timed out\n");
            }
        }
    }
    print_out(want_json(g, args), &out, text);
    EXIT_OK
}

/// `vibeke machine connect <m>`: bring the link up now (shared ssh connection, remote server
/// started if needed) and report it. The connection is shared by every client for
/// `ControlPersist` (60 s), so a following `vibeke ssh`/TUI attach is instant.
async fn machine_connect(g: &Global, args: &[String]) -> i32 {
    let Some(label) = positional(args, &[]) else {
        eprintln!("vibeke machine connect <machine> [--json]");
        return EXIT_USAGE;
    };
    let cfg = crate::commands::load_config();
    let target = target_for(&cfg, label);
    let (link, r) = connect_link(g, target.clone()).await;
    let ls = link.status();
    let code = match r {
        Ok(v) => {
            let out = json!({"machine": target.label, "connected": true, "link": link_json(&ls),
                             "server": {"version": v["version"], "panes": v["panes"]}});
            print_out(
                want_json(g, args),
                &out,
                format!(
                    "{}: connected{} (vibeke {}, {} pane(s))\n",
                    target.label,
                    ls.rtt_ms
                        .map(|r| format!(", rtt {r} ms"))
                        .unwrap_or_default(),
                    v["version"].as_str().unwrap_or("?"),
                    v["panes"]
                ),
            );
            EXIT_OK
        }
        Err(e) => unavailable(&target.label, e),
    };
    link.disconnect().await;
    code
}

/// `vibeke machine disconnect <m>`: close the shared ssh connection to the machine. Every link
/// through it ends (clients show the machine offline and reconnect with backoff; set
/// `auto_connect = false` to keep it out of the unified view). Remote panes keep running.
async fn machine_disconnect(g: &Global, args: &[String]) -> i32 {
    let Some(label) = positional(args, &[]) else {
        eprintln!("vibeke machine disconnect <machine> [--json]");
        return EXIT_USAGE;
    };
    let cfg = crate::commands::load_config();
    let target = target_for(&cfg, label);
    let (was, msg) = match target.control("exit").await {
        Ok(m) => (true, m),
        Err(e) => (false, format!("{e:#}")),
    };
    let out = json!({"machine": target.label, "disconnected": was, "detail": msg});
    print_out(
        want_json(g, args),
        &out,
        if was {
            format!(
                "{}: disconnected (remote panes keep running; open clients reconnect with backoff)\n",
                target.label
            )
        } else {
            format!("{}: not connected\n", target.label)
        },
    );
    EXIT_OK
}

/// `bootstrap = "remote-download"`: fetch and verify the signed release manifest here (never
/// unsigned: the opt-in does not apply, there is no local file to checksum), then let the
/// remote download the binary and check the manifest's sha256. `GITHUB_TOKEN` /
/// `VIBEKE_GITHUB_TOKEN` (private release repo) is used for the manifest and the asset lookup
/// on this machine, and sent to the remote only on stdin, never on a command line.
async fn ensure_via_download(
    target: &Target,
    probe: &bootstrap::Probe,
    upgrade_ok: bool,
    allow_downgrade: bool,
) -> anyhow::Result<bootstrap::Outcome> {
    use vk_remote::download;
    let base = download::release_base_url(vk_proto::VERSION);
    let token = download::github_token();
    let api = download::github_api_base();
    let (t2, base2) = (token.clone(), base.clone());
    let (manifest, sig) = tokio::task::spawn_blocking(move || {
        let manifest = download::fetch(&format!("{base2}/manifest.json"), t2.as_ref(), &api)?;
        let sig = download::fetch(&format!("{base2}/manifest.json.minisig"), t2.as_ref(), &api)
            .map_err(|e| {
                anyhow::anyhow!(
                    "no manifest.json.minisig ({e}); expected a signature by {}",
                    bootstrap::expected_keys_hint()
                )
            })?;
        anyhow::Ok((manifest, sig))
    })
    .await??;
    let m = bootstrap::verify_manifest(&manifest, &String::from_utf8_lossy(&sig))
        .map_err(|e| anyhow::anyhow!("release manifest from {base} is not trusted: {e}"))?;
    if m.version != vk_proto::VERSION {
        anyhow::bail!(
            "release manifest is for vibeke {}, this is {}",
            m.version,
            vk_proto::VERSION
        );
    }
    bootstrap::ensure_download(
        target,
        probe,
        &m,
        token.as_ref(),
        upgrade_ok,
        allow_downgrade,
    )
    .await
}

/// `vibeke machine upgrade <m> [--from <artifact> [--version v]] [--stage-only] [--force]`:
/// install or upgrade the remote binary to a **verified** artifact (signed, or
/// `VIBEKE_ALLOW_UNSIGNED=1` for development builds). Stages it into
/// `versions/<v>/` and re-checks its sha256 on the remote, then switches `current`
/// atomically and restarts the remote server (holders keep every pane alive). With
/// `--stage-only` nothing the remote runs changes.
async fn machine_upgrade(g: &Global, args: &[String]) -> i32 {
    let Some(label) = positional(args, &["--from", "--version"]) else {
        eprintln!(
            "vibeke machine upgrade <machine> [--from <artifact> [--version v]] [--stage-only] [--force]"
        );
        return EXIT_USAGE;
    };
    if let Err(e) = check_session(&g.session) {
        eprintln!("vibeke: {e:#}");
        return EXIT_USAGE;
    }
    let cfg = crate::commands::load_config();
    let target = target_for(&cfg, label);
    let probe = match bootstrap::probe(&target).await {
        Ok(p) => p,
        Err(e) => return unavailable(&target.label, format!("{e:#}")),
    };
    let artifact = match opt(args, "--from") {
        Some(p) => {
            let version = opt(args, "--version").unwrap_or_else(|| vk_proto::VERSION.into());
            match bootstrap::load_artifact(
                PathBuf::from(p),
                version,
                bootstrap::allow_unsigned_env(),
            ) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("vibeke: {e:#}");
                    return EXIT_API;
                }
            }
        }
        None => match artifact_for(&probe.target()) {
            Some(a) => a,
            None => {
                eprintln!(
                    "vibeke: no verified vibeke {} artifact for {} (build one with `mise run dist`, or pass --from <file> with a SHA256SUMS next to it)",
                    vk_proto::VERSION,
                    probe.target()
                );
                return EXIT_API;
            }
        },
    };
    if artifact.trust != bootstrap::Trust::Signed {
        eprintln!(
            "vibeke: {}",
            bootstrap::unsigned_warning(&artifact.path, &artifact.sha256)
        );
    }
    let have = probe.version.clone();
    if have.as_deref() == Some(artifact.version.as_str()) && !flag(args, "--force") {
        println!(
            "{}: already runs vibeke {} (--force reinstalls)",
            target.label, artifact.version
        );
        return EXIT_OK;
    }
    let staged = match bootstrap::stage(&target, &artifact).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("vibeke: staging on {} failed: {e:#}", target.label);
            return EXIT_API;
        }
    };
    println!(
        "{}: staged vibeke {} at {staged} (sha256 {} verified on the remote)",
        target.label, artifact.version, artifact.sha256
    );
    if flag(args, "--stage-only") {
        println!(
            "{}: --stage-only: still running vibeke {}",
            target.label,
            have.as_deref().unwrap_or("(none)")
        );
        return EXIT_OK;
    }
    if let Err(e) = bootstrap::activate(&target, &artifact).await {
        eprintln!("vibeke: activating on {} failed: {e:#}", target.label);
        return EXIT_API;
    }
    println!(
        "{}: {} {} → {}",
        target.label,
        if have.is_some() {
            "upgraded"
        } else {
            "installed"
        },
        have.as_deref().unwrap_or("(none)"),
        artifact.version
    );
    if have.is_some() {
        // Restart the remote server on the new binary; holders keep every pane alive.
        let restart = format!(
            "{} server restart --session {}",
            vk_remote::ssh::sh_quote(bootstrap::REMOTE_BIN),
            vk_remote::ssh::sh_quote(&g.session)
        );
        if let Err(e) = target.run(&restart, None).await {
            eprintln!(
                "vibeke: {}: server restart failed ({e:#}); it picks up the new binary on its next start",
                target.label
            );
        }
    }
    EXIT_OK
}

// ---- cross-machine agent list (06 A5) ----------------------------------------------------

async fn agents_of<S>(c: &mut vk_cli::client::Client<S>) -> Result<(Value, Value), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let a = c
        .call("agent.list", json!({}))
        .await
        .map_err(|e| e.to_string())?;
    // Older servers without interaction.list still list their runs.
    let i = c
        .call("interaction.list", json!({"status": "open"}))
        .await
        .unwrap_or_else(|_| json!({"interactions": []}));
    Ok((a, i))
}

/// `vibeke agent list --all-machines [--json]`: every run on this machine and every saved
/// remote machine, sorted by attention (needs_approval > needs_answer > error > done >
/// working > idle). Unreachable machines are listed as offline, never skipped silently.
pub async fn agent_list_all(g: &Global, args: &[String]) -> i32 {
    let cfg = crate::commands::load_config();
    let socket = vk_cli::client::socket_path(&g.session, g.socket.as_deref());
    let local = match vk_cli::client::connect_or_spawn(&g.session, &socket, g.no_spawn).await {
        Ok(s) => agents_of(&mut vk_cli::client::Client::new(s)).await,
        Err(e) => Err(format!("{e:#}")),
    };
    let mut tasks = Vec::new();
    for m in &cfg.remote.machine {
        let target = Target::parse(&m.label, &m.address);
        let session = g.session.clone();
        tasks.push(tokio::spawn(async move {
            let label = target.label.clone();
            let link = Link::new(target, &session);
            let r = tokio::time::timeout(std::time::Duration::from_secs(15), async {
                check_session(&session).map_err(|e| format!("{e:#}"))?;
                let s = link.open().await.map_err(|e| format!("{e:#}"))?;
                agents_of(&mut vk_cli::client::Client::new(s)).await
            })
            .await
            .unwrap_or_else(|_| Err(format!("machine {label} did not answer (offline?)")));
            link.disconnect().await;
            vk_remote::agents::MachineAgents {
                machine: label,
                result: r,
            }
        }));
    }
    let mut all = vec![vk_remote::agents::MachineAgents {
        machine: crate::commands::hostname(),
        result: local,
    }];
    for t in tasks {
        if let Ok(m) = t.await {
            all.push(m);
        }
    }
    let v = vk_remote::agents::combine(all);
    let text = vk_remote::agents::render_table(&v);
    print_out(want_json(g, args), &v, text);
    EXIT_OK
}

// ---- attach-file (06 A10) ------------------------------------------------------------------

fn shell_escape(p: &str) -> String {
    let mut out = String::new();
    for c in p.chars() {
        if c.is_whitespace() || "\\'\"()[]{}&;|<>*?$`!#".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Upload `data` as `name` over `c` (chunked `blob.*`); returns the commit result.
async fn upload_blob<S>(
    c: &mut vk_cli::client::Client<S>,
    name: &str,
    data: &[u8],
    unpack: bool,
) -> Result<Value, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use base64::Engine;
    let begun = c
        .call("blob.begin", json!({"name": name, "size": data.len()}))
        .await
        .map_err(|e| e.to_string())?;
    let id = begun["upload_id"]
        .as_str()
        .ok_or("blob.begin: no upload_id")?
        .to_string();
    let chunk = 512 * 1024;
    let mut off = 0usize;
    while off < data.len() {
        let end = (off + chunk).min(data.len());
        let b64 = base64::engine::general_purpose::STANDARD.encode(&data[off..end]);
        if let Err(e) = c
            .call(
                "blob.append",
                json!({"upload_id": id, "offset": off, "data_b64": b64}),
            )
            .await
        {
            let _ = c.call("blob.abort", json!({"upload_id": id})).await;
            return Err(e.to_string());
        }
        off = end;
    }
    let mut p = json!({"upload_id": id});
    if unpack {
        p["unpack"] = json!("tar");
    }
    let done = c.call("blob.commit", p).await.map_err(|e| e.to_string())?;
    if unpack && done["unpacked"].as_bool() != Some(true) {
        return Err("this machine's vibeke cannot unpack directory drops (upgrade it)".into());
    }
    Ok(done)
}

/// `vibeke attach-file <path> [--pane [machine/]pane] [--machine m] [--no-paste] [--json]`:
/// copy a local file (or directory) into the pane's machine inbox and paste the resulting
/// path into the pane, as a drop would (06 A10/A11). Any file type; size capped by
/// `paste.max_auto_bytes`. Without `--pane` it only uploads and prints the path.
pub async fn attach_file(g: &Global, args: &[String]) -> i32 {
    let usage =
        "vibeke attach-file <path> [--pane [machine/]pane] [--machine m] [--no-paste] [--json]";
    let Some(path) = positional(args, &["--pane", "--machine"]) else {
        eprintln!("{usage}");
        return EXIT_USAGE;
    };
    let mut machine = opt(args, "--machine").or_else(|| g.machine.clone());
    let mut pane = opt(args, "--pane");
    if let Some(p) = pane.clone()
        && let Some((m, rest)) = p.split_once('/')
        && !m.is_empty()
    {
        machine = Some(m.to_string());
        pane = Some(rest.to_string());
    }
    let cfg = crate::commands::load_config();
    let limit = cfg.paste.max_auto_bytes.0;
    let src = std::path::Path::new(path);
    let meta = match std::fs::metadata(src) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("vibeke attach-file: {path}: {e}");
            return EXIT_USAGE;
        }
    };
    let base = src
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let (name, data, dir) = if meta.is_dir() {
        let mut tar = Vec::new();
        if let Err(e) = vk_remote::inbox::pack_dir(src, &mut tar, limit) {
            eprintln!("vibeke attach-file: {e:#}");
            return EXIT_USAGE;
        }
        (format!("{base}.tar"), tar, true)
    } else {
        if meta.len() > limit {
            eprintln!(
                "vibeke attach-file: {path} is {} bytes, over paste.max_auto_bytes ({limit})",
                meta.len()
            );
            return EXIT_USAGE;
        }
        match std::fs::read(src) {
            Ok(d) => (base.clone(), d, false),
            Err(e) => {
                eprintln!("vibeke attach-file: {path}: {e}");
                return EXIT_USAGE;
            }
        }
    };
    let run = async |mut c: vk_cli::client::Client<crate::AnyStream>| -> Result<Value, String> {
        let done = upload_blob(&mut c, &name, &data, dir).await?;
        let landed = done["path_on_machine"]
            .as_str()
            .ok_or("blob.commit: no path")?
            .to_string();
        if let Some(p) = &pane {
            let ns = match &machine {
                Some(m) => format!("ssh:{m}"),
                None => "local".into(),
            };
            let _ = c
                .call(
                    "paste.translated",
                    json!({"pane": p, "target_namespace": ns, "files": [{
                        "blob": done["hash"], "bytes": data.len(), "local_name": base, "dir": dir}]}),
                )
                .await;
            if !flag(args, "--no-paste") {
                c.call(
                    "pane.send_text",
                    json!({"pane": p, "text": shell_escape(&landed), "paste": "bracketed"}),
                )
                .await
                .map_err(|e| e.to_string())?;
            }
        }
        Ok(
            json!({"path": landed, "pane": pane, "machine": machine, "bytes": data.len(), "dir": dir}),
        )
    };
    let stream: anyhow::Result<crate::AnyStream> = match &machine {
        Some(m) => {
            let target = target_for(&cfg, m);
            let link = Link::new(target, &g.session);
            let r = async {
                check_session(&g.session)?;
                link.open_class(Class::Blob).await
            }
            .await;
            // The CLI exits after one command; keep the ssh link alive until then.
            std::mem::forget(link);
            r.map(|s| Box::new(s) as crate::AnyStream)
        }
        None => {
            let socket = vk_cli::client::socket_path(&g.session, g.socket.as_deref());
            vk_cli::client::connect_or_spawn(&g.session, &socket, g.no_spawn)
                .await
                .map(|s| Box::new(s) as crate::AnyStream)
        }
    };
    let stream = match stream {
        Ok(s) => s,
        Err(e) => return unavailable(machine.as_deref().unwrap_or("local"), format!("{e:#}")),
    };
    match run(vk_cli::client::Client::new(stream)).await {
        Ok(v) => {
            let text = format!("{}\n", v["path"].as_str().unwrap_or(""));
            print_out(want_json(g, args), &v, text);
            EXIT_OK
        }
        Err(e) => {
            eprintln!("vibeke attach-file: {e}");
            EXIT_API
        }
    }
}

// ---- saved machines --------------------------------------------------------------------------

pub fn machine_cmd(_g: &Global, args: &[String]) -> i32 {
    let path = vk_config::config_path();
    let src = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = match src.parse() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            return EXIT_API;
        }
    };
    let verb = args.first().map(String::as_str).unwrap_or("list");
    match verb {
        "list" => {
            let cfg = crate::commands::load_config();
            for m in &cfg.remote.machine {
                println!(
                    "{:<12} {:<32} auto_connect={}",
                    m.label, m.address, m.auto_connect
                );
            }
            EXIT_OK
        }
        "add" => {
            let (Some(label), Some(address)) = (args.get(1), args.get(2)) else {
                eprintln!("vibeke machine add <label> <user@host[:port]> [--auto-connect]");
                return EXIT_USAGE;
            };
            let auto = args.iter().any(|a| a == "--auto-connect");
            let remote = doc
                .entry("remote")
                .or_insert(toml_edit::table())
                .as_table_mut()
                .map(|t| t as *mut toml_edit::Table);
            let Some(remote) = remote else {
                return EXIT_API;
            };
            // SAFETY: pointer from a live &mut into `doc`, used immediately.
            let remote = unsafe { &mut *remote };
            let arr = remote
                .entry("machine")
                .or_insert(toml_edit::Item::ArrayOfTables(
                    toml_edit::ArrayOfTables::new(),
                ));
            let Some(arr) = arr.as_array_of_tables_mut() else {
                eprintln!("[remote.machine] has an unexpected shape");
                return EXIT_API;
            };
            if arr
                .iter()
                .any(|t| t.get("label").and_then(|v| v.as_str()) == Some(label.as_str()))
            {
                eprintln!("machine {label} already exists");
                return EXIT_API;
            }
            let mut t = toml_edit::Table::new();
            t["label"] = toml_edit::value(label.as_str());
            t["address"] = toml_edit::value(address.as_str());
            t["auto_connect"] = toml_edit::value(auto);
            arr.push(t);
            if let Some(d) = path.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            match std::fs::write(&path, doc.to_string()) {
                Ok(()) => {
                    println!("added {label} ({address}) to {}", path.display());
                    EXIT_OK
                }
                Err(e) => {
                    eprintln!("{e}");
                    EXIT_API
                }
            }
        }
        "remove" | "rm" => {
            let Some(label) = args.get(1) else {
                return EXIT_USAGE;
            };
            if let Some(arr) = doc
                .get_mut("remote")
                .and_then(|r| r.get_mut("machine"))
                .and_then(|m| m.as_array_of_tables_mut())
            {
                arr.retain(|t| t.get("label").and_then(|v| v.as_str()) != Some(label.as_str()));
            }
            match std::fs::write(&path, doc.to_string()) {
                Ok(()) => EXIT_OK,
                Err(e) => {
                    eprintln!("{e}");
                    EXIT_API
                }
            }
        }
        "status" | "doctor" => {
            let Some(label) = args.get(1).cloned() else {
                return EXIT_USAGE;
            };
            let cfg = crate::commands::load_config();
            let target = target_for(&cfg, &label);
            let rt = tokio::runtime::Handle::current();
            let r = std::thread::spawn(move || rt.block_on(bootstrap::probe(&target))).join();
            match r {
                Ok(Ok(p)) => {
                    println!(
                        "{label}: {} {} home={} vibeke={} libc={}",
                        p.os,
                        p.arch,
                        p.home,
                        p.version.as_deref().unwrap_or("not installed"),
                        p.libc
                    );
                    EXIT_OK
                }
                Ok(Err(e)) => {
                    eprintln!("{label}: {e:#}");
                    EXIT_API
                }
                Err(_) => EXIT_API,
            }
        }
        _ => {
            eprintln!(
                "vibeke machine list|add|remove|show|connect|disconnect|upgrade|status|doctor"
            );
            EXIT_USAGE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vk_remote::link::valid_session_name;

    #[test]
    fn session_names_are_validated() {
        for ok in ["default", "a.b-c_d", "x", &"a".repeat(64)] {
            assert!(valid_session_name(ok), "{ok}");
        }
        for bad in [
            "",
            "a b",
            "a;rm -rf ~",
            "$(id)",
            "a'b",
            "a/b",
            "a\nb",
            &"a".repeat(65),
        ] {
            assert!(!valid_session_name(bad), "{bad:?}");
            assert!(check_session(bad).is_err());
        }
    }
}
