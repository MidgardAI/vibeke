//! Remote machines over SSH (06 Part A): `vibeke ssh <host>`, `vibeke bridge` (remote side),
//! saved machines (`vibeke machine …`), and `--machine` forwarding (never falls back to local).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use vk_cli::{EXIT_API, EXIT_OK, EXIT_USAGE, Global};
use vk_remote::bootstrap::{self, Artifact};
use vk_remote::{Mux, Target};

/// Remote side of the link: stdio mux → this machine's server socket.
pub async fn bridge(g: &Global, _args: &[String]) -> i32 {
    let socket = vk_cli::client::socket_path(&g.session, None);
    let session = g.session.clone();
    let r = vk_remote::run_bridge(socket.clone(), move || {
        let session = session.clone();
        let socket = socket.clone();
        async move {
            vk_cli::client::connect_or_spawn(&session, &socket, false).await?;
            Ok(())
        }
    })
    .await;
    match r {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("bridge: {e:#}");
            EXIT_API
        }
    }
}

fn target_for(cfg: &vk_config::Config, host: &str) -> Target {
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

/// Session names reach remote shell commands and file names: `[A-Za-z0-9_.-]{1,64}`.
pub fn valid_session_name(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
}

fn check_session(s: &str) -> anyhow::Result<()> {
    if !valid_session_name(s) {
        anyhow::bail!("invalid session name {s:?}: use 1-64 characters from [A-Za-z0-9_.-]");
    }
    Ok(())
}

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

/// A machine connection shared by every channel the TUI/CLI opens; reconnects on demand.
#[derive(Clone)]
pub struct Link {
    target: Target,
    session: String,
    mux: Arc<Mutex<Option<(Mux, tokio::process::Child)>>>,
}

impl Link {
    pub fn new(target: Target, session: &str) -> Self {
        Link {
            target,
            session: session.to_string(),
            mux: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn open(&self) -> anyhow::Result<tokio::io::DuplexStream> {
        check_session(&self.session)?;
        let mut g = self.mux.lock().await;
        if g.as_ref().is_none_or(|(m, _)| m.is_closed()) {
            let (m, child) = tokio::time::timeout(
                Duration::from_secs(15),
                self.target.bridge(bootstrap::REMOTE_BIN, &self.session),
            )
            .await
            .map_err(|_| anyhow::anyhow!("ssh {} timed out", self.target.address))??;
            *g = Some((m, child));
        }
        let m = g.as_ref().map(|(m, _)| m.clone()).unwrap();
        drop(g);
        match tokio::time::timeout(Duration::from_secs(10), m.open("socket")).await {
            Ok(Ok(s)) => Ok(s),
            Ok(Err(e)) => {
                *self.mux.lock().await = None;
                Err(e)
            }
            Err(_) => {
                *self.mux.lock().await = None;
                anyhow::bail!("machine {} did not answer (offline?)", self.target.label)
            }
        }
    }

    #[allow(dead_code)]
    pub async fn rtt_ms(&self) -> Option<u64> {
        let g = self.mux.lock().await;
        g.as_ref()
            .map(|(m, _)| m.stats().rtt_us.load(std::sync::atomic::Ordering::Relaxed) / 1000)
    }
}

fn spec_for(link: Link) -> vk_tui::app::MachineSpec {
    let label = link.target.label.clone();
    vk_tui::app::MachineSpec {
        label,
        local: false,
        connect: Box::new(move || {
            let link = link.clone();
            Box::pin(async move {
                let s = link.open().await?;
                Ok(Box::new(s) as vk_tui::app::Stream)
            })
        }),
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

/// `vibeke ssh <host> [--upgrade] [--yes] [--remote-session s]`: probe, install/upgrade the
/// remote binary (no sudo), then attach the TUI to the remote server over the bridge.
pub async fn ssh(g: &Global, args: &[String]) -> i32 {
    let Some(host) = args.iter().find(|a| !a.starts_with("--")) else {
        eprintln!("vibeke ssh <host|label> [--upgrade] [--no-local]");
        return EXIT_USAGE;
    };
    if let Err(e) = check_session(&g.session) {
        eprintln!("vibeke: {e:#}");
        return EXIT_USAGE;
    }
    let upgrade = args.iter().any(|a| a == "--upgrade" || a == "--yes");
    let cfg = crate::commands::load_config();
    let target = target_for(&cfg, host);
    let auto_upgrade = cfg
        .remote
        .machine
        .iter()
        .find(|m| m.label == target.label)
        .is_some_and(|m| m.auto_upgrade);
    eprintln!("vibeke: probing {} …", target.address);
    let probe = match bootstrap::probe(&target).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e:#}");
            return EXIT_API;
        }
    };
    let artifact = artifact_for(&probe.target());
    if let Some(a) = &artifact
        && a.trust != bootstrap::Trust::Signed
    {
        eprintln!(
            "vibeke: {}",
            bootstrap::unsigned_warning(&a.path, &a.sha256)
        );
    }
    match bootstrap::ensure(&target, &probe, artifact.as_ref(), upgrade || auto_upgrade).await {
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
    };
    let _ = remote_index;
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
            eprintln!("vibeke machine list|add|remove|status");
            EXIT_USAGE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
