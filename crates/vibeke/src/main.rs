//! The single `vibeke` binary (01 §1.1): TUI client, CLI, server, holder, hook shim, bridge.

use serde_json::{Value, json};
use std::path::PathBuf;
use vk_cli::client;
use vk_cli::{EXIT_NO_SERVER, EXIT_OK, EXIT_USAGE, Global};

mod commands;
mod debug;
mod doctor;
mod integration;
mod remote;

pub use remote::specs as remote_specs;

const HELP: &str = "vibeke — a terminal workspace for supervising coding agents

usage:
  vibeke                          attach the TUI (spawns the server if needed)
  vibeke attach [--session s]
  vibeke ssh <host>               attach to a remote machine over SSH (installs vibeke there)
  vibeke <noun> <verb> [args]     API commands (vibeke <noun> for help)
  vibeke notify <title> [body]    notification from a pane or script
  vibeke import herdr [--config] [--session] [--dry-run]
  vibeke integration install|status|uninstall|doctor <claude|codex>
  vibeke doctor                   diagnose install, sockets, integrations, terminal, remote
  vibeke update [--check]         replace the binary and restart the server (panes survive)
  vibeke server [start|stop|status|restart]
  vibeke api call <method> [json]
  vibeke --skill | --default-config | --version

global flags: --session NAME  --machine LABEL  --socket PATH  --json  --pretty  --quiet  --no-spawn  --timeout MS
nouns: ";

fn parse_global(args: &mut Vec<String>) -> Result<Global, String> {
    let mut g = Global {
        session: std::env::var("VIBEKE_SESSION").unwrap_or_else(|_| "default".into()),
        ..Default::default()
    };
    // `vibeke import herdr --session` uses `--session` as a plain flag (08 §12).
    let importing = args.first().map(String::as_str) == Some("import");
    let mut i = 0;
    while i < args.len() {
        let take = |args: &mut Vec<String>, i: usize| -> Result<String, String> {
            if i + 1 >= args.len() {
                return Err(format!("{} needs a value", args[i]));
            }
            let v = args.remove(i + 1);
            args.remove(i);
            Ok(v)
        };
        match args[i].as_str() {
            "--session" if importing => i += 1,
            "--session" => g.session = take(args, i)?,
            "--machine" => g.machine = Some(take(args, i)?),
            "--socket" => g.socket = Some(PathBuf::from(take(args, i)?)),
            "--timeout" => g.timeout_ms = take(args, i)?.parse().ok(),
            "--json" => {
                g.json = Some(true);
                args.remove(i);
            }
            "--pretty" => {
                g.json = Some(false);
                args.remove(i);
            }
            "--quiet" | "-q" => {
                g.quiet = true;
                args.remove(i);
            }
            "--no-spawn" => {
                g.no_spawn = true;
                args.remove(i);
            }
            "--" => break,
            _ => i += 1,
        }
    }
    Ok(g)
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // The holder must fork before any threads exist.
    if args.first().map(String::as_str) == Some("hold") {
        let get = |k: &str| {
            args.iter()
                .position(|a| a == k)
                .and_then(|i| args.get(i + 1))
                .map(PathBuf::from)
        };
        let Some(spec) = get("--spec") else {
            eprintln!("vibeke hold --spec FILE [--log FILE]");
            std::process::exit(EXIT_USAGE);
        };
        let r = vk_hold::main_daemon(&spec, get("--log").as_deref());
        std::process::exit(if r.is_ok() { 0 } else { 1 });
    }
    if args.first().map(String::as_str) == Some("debug")
        && args.get(1).map(String::as_str) == Some("ptyshot")
    {
        std::process::exit(debug::ptyshot(&args[2..]));
    }
    if args.first().map(String::as_str) == Some("hook") {
        // Sync, no runtime: must stay within the hook latency budget (04 §7.5).
        std::process::exit(commands::hook(&args[1..]));
    }
    let g = match parse_global(&mut args) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(EXIT_USAGE);
        }
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .build()
        .expect("tokio runtime");
    let code = rt.block_on(dispatch(g, args));
    rt.shutdown_timeout(std::time::Duration::from_millis(100));
    std::process::exit(code);
}

async fn dispatch(g: Global, args: Vec<String>) -> i32 {
    let first = args.first().map(String::as_str);
    match first {
        None | Some("attach") => commands::attach(&g, &args).await,
        Some("--help" | "-h" | "help") => {
            println!("{HELP}{}", vk_cli::nouns().join(" "));
            EXIT_OK
        }
        Some("--version" | "-V" | "version") => {
            println!("vibeke {}", vk_proto::VERSION);
            EXIT_OK
        }
        Some("--skill") => {
            print!("{}", commands::SKILL);
            EXIT_OK
        }
        Some("--default-config") => {
            print!("{}", vk_config::default_config_toml());
            EXIT_OK
        }
        Some("server") => commands::server(&g, &args[1..]).await,
        Some("bridge") => commands::bridge(&g, &args[1..]).await,
        Some("ssh") => commands::ssh(&g, &args[1..]).await,
        Some("notify") => commands::notify(&g, &args[1..]).await,
        Some("import") => commands::import(&g, &args[1..]).await,
        Some("integration") => commands::integration(&g, &args[1..]).await,
        Some("doctor") => commands::doctor(&g, &args[1..]).await,
        Some("update") => commands::update(&g, &args[1..]).await,
        Some("config") => commands::config(&g, &args[1..]),
        Some("keys") => commands::keys(&g, &args[1..]),
        Some("machine") => remote::machine_cmd(&g, &args[1..]),
        Some("debug") if args.get(1).map(String::as_str) == Some("latency") => {
            debug::latency(&g, &args[2..]).await
        }
        Some("debug") if args.get(1).map(String::as_str) == Some("bandwidth") => {
            debug::bandwidth(&g, &args[2..]).await
        }
        Some("api") if args.get(1).map(String::as_str) == Some("call") => {
            let Some(method) = args.get(2) else {
                eprintln!("vibeke api call <method> [json]");
                return EXIT_USAGE;
            };
            let params: Value = match args.get(3).map(|s| serde_json::from_str(s)) {
                Some(Ok(v)) => v,
                Some(Err(e)) => {
                    eprintln!("invalid json: {e}");
                    return EXIT_USAGE;
                }
                None => json!({}),
            };
            {
                let gr = &g;
                with_client(gr, |mut c| async move {
                    vk_cli::run_api(&mut c, gr, method, params).await
                })
                .await
            }
        }
        Some(noun) => {
            let Some(verb) = args.get(1) else {
                if vk_cli::nouns().contains(&noun) {
                    print!("{}", vk_cli::noun_help(noun));
                    return EXIT_OK;
                }
                eprintln!("unknown command `{noun}`; see vibeke --help");
                return EXIT_USAGE;
            };
            let Some((method, positional)) = vk_cli::lookup(noun, verb) else {
                eprintln!(
                    "unknown command `{noun} {verb}`\n{}",
                    vk_cli::noun_help(noun)
                );
                return EXIT_USAGE;
            };
            let params = match vk_cli::build_params(positional, &args[2..]) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{e}\n{}", vk_cli::noun_help(noun));
                    return EXIT_USAGE;
                }
            };
            {
                let gr = &g;
                with_client(gr, |mut c| async move {
                    vk_cli::run_api(&mut c, gr, method, params).await
                })
                .await
            }
        }
    }
}

pub type AnyStream = Box<dyn vk_remote::mux::Stream>;

/// Connect to the session's server (spawning it unless --no-spawn) and run `f`. With
/// `--machine`, the connection is a channel to that machine's server — never a local fallback
/// (06 A6).
pub async fn with_client<F, Fut>(g: &Global, f: F) -> i32
where
    F: FnOnce(client::Client<AnyStream>) -> Fut,
    Fut: std::future::Future<Output = i32>,
{
    if let Some(m) = &g.machine {
        return match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            remote::machine_stream(g, m),
        )
        .await
        {
            Ok(Ok(s)) => f(client::Client::new(Box::new(s) as AnyStream)).await,
            Ok(Err(e)) => {
                eprintln!(
                    "{}",
                    json!({"error": {"kind": "remote_unavailable", "message": format!("{e:#}"), "details": {"machine": m}, "retryable": true}})
                );
                vk_cli::EXIT_API
            }
            Err(_) => {
                eprintln!(
                    "{}",
                    json!({"error": {"kind": "remote_unavailable", "message": format!("machine {m} offline"), "details": {"machine": m}, "retryable": true}})
                );
                vk_cli::EXIT_API
            }
        };
    }
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    match client::connect_or_spawn(&g.session, &socket, g.no_spawn).await {
        Ok(s) => f(client::Client::new(Box::new(s) as AnyStream)).await,
        Err(e) => {
            eprintln!(
                "{}",
                json!({"error": {"kind": "server_unavailable", "message": format!("{e:#}")}})
            );
            EXIT_NO_SERVER
        }
    }
}
