//! Top-level commands that aren't plain API calls.

use serde_json::json;
use std::path::{Path, PathBuf};
use vk_cli::client::{self, Client};
use vk_cli::{EXIT_API, EXIT_OK, EXIT_USAGE, Global};
use vk_server::paths::Paths;

pub const SKILL: &str = include_str!("../skill/SKILL.md");

pub fn load_config() -> vk_config::Config {
    match vk_config::Config::load(vk_config::config_path()) {
        Ok((c, _)) => c,
        Err(e) => {
            eprintln!("config error (using defaults): {e}");
            vk_config::Config::default()
        }
    }
}

// ---- attach ---------------------------------------------------------------------------------

pub async fn attach(g: &Global, args: &[String]) -> i32 {
    // `--readonly`: watch without input (07 §5.3); the server refuses this client's input and
    // mutating commands.
    let readonly = args.iter().any(|a| a == "--readonly" || a == "--read-only");
    if let Some(m) = &g.machine {
        // `vibeke --machine host attach` attaches to that machine (with the remote focused).
        return crate::remote::ssh(g, std::slice::from_ref(m)).await;
    }
    if std::env::var("VIBEKE").as_deref() == Ok("1")
        && std::env::var("VIBEKE_SESSION").as_deref() == Ok(g.session.as_str())
    {
        eprintln!(
            "already inside vibeke session `{}` (nesting would loop); use --session to attach another",
            g.session
        );
        return EXIT_USAGE;
    }
    let config = load_config();
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    if let Err(e) = client::connect_or_spawn(&g.session, &socket, g.no_spawn).await {
        eprintln!("{e:#}");
        return vk_cli::EXIT_NO_SERVER;
    }
    let mut specs = vec![local_spec_with(&g.session, socket.clone(), readonly)];
    if !readonly {
        specs.extend(crate::remote_specs(&config, g));
    }
    let opts = vk_tui::app::Opts {
        session: g.session.clone(),
        config,
        initial_machine: 0,
    };
    match vk_tui::app::run(opts, specs).await {
        Ok(reason) => {
            if reason != "detached" {
                eprintln!("[{reason}]");
            } else {
                eprintln!("[detached from session {}]", g.session);
            }
            EXIT_OK
        }
        Err(e) => {
            eprintln!("vibeke: {e:#}");
            EXIT_API
        }
    }
}

pub fn local_spec(session: &str, socket: PathBuf) -> vk_tui::app::MachineSpec {
    local_spec_with(session, socket, false)
}

/// Say `client.hello {readonly: true}` on a fresh connection before the TUI uses it. Reads the
/// response byte by byte so nothing after it is consumed.
async fn readonly_prelude(mut s: tokio::net::UnixStream) -> anyhow::Result<tokio::net::UnixStream> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let hello = json!({"jsonrpc": "2.0", "id": "readonly-hello", "method": "client.hello", "params": {"client": "vibeke-tui", "version": vk_proto::VERSION, "api": vk_proto::API_VERSION, "kind": "tui", "readonly": true}});
    s.write_all(format!("{hello}\n").as_bytes()).await?;
    let mut b = [0u8; 1];
    loop {
        if s.read(&mut b).await? == 0 {
            anyhow::bail!("server closed the connection");
        }
        if b[0] == b'\n' {
            return Ok(s);
        }
    }
}

pub fn local_spec_with(session: &str, socket: PathBuf, readonly: bool) -> vk_tui::app::MachineSpec {
    let session = session.to_string();
    vk_tui::app::MachineSpec {
        label: hostname(),
        local: true,
        connect: Box::new(move || {
            let socket = socket.clone();
            let session = session.clone();
            Box::pin(async move {
                let mut s = client::connect_or_spawn(&session, &socket, false).await?;
                if readonly {
                    s = readonly_prelude(s).await?;
                }
                Ok(Box::new(s) as vk_tui::app::Stream)
            })
        }),
        bulk: None,
        link: None,
    }
}

pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes.
    let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if r != 0 {
        return "local".into();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let h = String::from_utf8_lossy(&buf[..end]).into_owned();
    h.split('.').next().unwrap_or("local").to_string()
}

// ---- server ---------------------------------------------------------------------------------

pub async fn server(g: &Global, args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None | Some("start") | Some("--foreground") | Some("run") => run_server(g).await,
        // Local: the server execs itself (`server.restart`, holders untouched, same pid).
        Some("restart") if g.machine.is_none() => restart_local(g, &args[1..]).await,
        Some("restart") => {
            let _ = crate::with_client(g, |mut c| async move {
                vk_cli::run_api(&mut c, g, "server.stop", json!({})).await
            })
            .await;
            if g.machine.is_some() {
                // The remote bridge starts the remote server on the next connection.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                return crate::with_client(g, |mut c| async move {
                    let code = vk_cli::run_api(&mut c, g, "server.status", json!({})).await;
                    if code == EXIT_OK {
                        eprintln!("remote server restarted");
                    }
                    code
                })
                .await;
            }
            let socket = client::socket_path(&g.session, g.socket.as_deref());
            for _ in 0..50 {
                if tokio::net::UnixStream::connect(&socket).await.is_err() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            match client::connect_or_spawn(&g.session, &socket, false).await {
                Ok(_) => {
                    println!("server restarted");
                    EXIT_OK
                }
                Err(e) => {
                    eprintln!("{e:#}");
                    EXIT_API
                }
            }
        }
        Some(verb) => {
            let method = match verb {
                "stop" => "server.stop",
                "status" => "server.status",
                "reload-config" => "server.reload_config",
                _ => {
                    eprintln!("vibeke server [start|stop|status|restart|reload-config]");
                    return EXIT_USAGE;
                }
            };
            let params = vk_cli::build_params(&[], &args[1..]).unwrap_or(json!({}));
            let mut g2 = g.clone();
            if verb != "status" {
                g2.no_spawn = true;
            }
            {
                let gr = &g2;
                crate::with_client(gr, |mut c| async move {
                    vk_cli::run_api(&mut c, gr, method, params).await
                })
                .await
            }
        }
    }
}

/// `vibeke server restart [--binary PATH]`: `server.restart`, then wait until the new image
/// answers.
async fn restart_local(g: &Global, args: &[String]) -> i32 {
    let params = match vk_cli::build_params(&[], args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}\nvibeke server restart [--binary PATH]");
            return EXIT_USAGE;
        }
    };
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    let s = match client::connect(&socket).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("server not running: {e:#}");
            return vk_cli::EXIT_NO_SERVER;
        }
    };
    let mut c = Client::new(s);
    let before = match c.hello("cli").await {
        Ok(_) => c
            .call("server.status", json!({}))
            .await
            .ok()
            .and_then(|v| v["uptime_ms"].as_u64()),
        Err(e) => {
            vk_cli::print_error(&e);
            return vk_cli::exit_code_for(&e);
        }
    };
    let r = match c.call("server.restart", params).await {
        Ok(r) => r,
        Err(e) => {
            vk_cli::print_error(&e);
            return vk_cli::exit_code_for(&e);
        }
    };
    drop(c);
    // The same pid comes back with a fresh uptime once the new image serves.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Ok(s) = client::connect(&socket).await {
            let mut c = Client::new(s);
            if c.hello("cli").await.is_ok()
                && let Ok(st) = c.call("server.status", json!({})).await
                && st["uptime_ms"].as_u64() < before
            {
                if g.json == Some(true) || !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                    println!(
                        "{}",
                        json!({"restarted": true, "pid": st["pid"], "binary": r["binary"], "panes": st["panes"]})
                    );
                } else {
                    println!(
                        "server restarted (pid {}, {} panes)",
                        st["pid"], st["panes"]
                    );
                }
                return EXIT_OK;
            }
        }
        if std::time::Instant::now() > deadline {
            eprintln!(
                "server did not come back within 15 s (see {})",
                Paths::new(&g.session).logs().join("server.log").display()
            );
            return EXIT_API;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

async fn run_server(g: &Global) -> i32 {
    // 09 §3.1: everything the server creates is private; pane children get the user's umask.
    vk_server::paths::harden_umask();
    let paths = Paths::new(&g.session);
    if let Err(e) = paths.ensure() {
        eprintln!("{e}");
        return EXIT_API;
    }
    let filter = tracing_subscriber::EnvFilter::try_from_env("VIBEKE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
    let cfg = load_config();
    let bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("vibeke"));
    let env: Vec<(String, String)> = std::env::vars().collect();
    let mut opts = vk_server::ServerOpts {
        session: g.session.clone(),
        machine: hostname(),
        bin: stable_bin(&bin),
        hold_args: vec!["hold".into()],
        default_shell: Some(cfg.terminal.default_shell.clone()).filter(|s| !s.is_empty()),
        env,
        shims: cfg.agents.shims,
    };
    opts.env
        .extend(cfg.terminal.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    if opts.shims {
        let _ = vk_server::agents::install_shims(&opts.bin);
    }
    // The state dir's writer lock, for the server's whole life (02): a running
    // `doctor --rebuild-index` holds it, and a server that is still shutting down may for a
    // moment, so wait a little, then refuse.
    let _state_lock = match paths.lock_state(std::time::Duration::from_secs(10)) {
        Ok(Some(l)) => l,
        Ok(None) => {
            eprintln!(
                "server: the state of session `{}` is locked by another process ({}): `vibeke doctor --rebuild-index` or another server is using it; retry when it is done",
                g.session,
                paths.state_lock().display()
            );
            return EXIT_API;
        }
        Err(e) => {
            eprintln!("server: lock {}: {e}", paths.state_lock().display());
            return EXIT_API;
        }
    };
    let server = match vk_server::Server::new(paths.clone(), opts) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("server: {e:#}");
            return EXIT_API;
        }
    };
    let listener = match vk_server::run::bind(&paths.socket()) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("server: {e:#}");
            return EXIT_API;
        }
    };
    match vk_server::run::serve(server, listener).await {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("server: {e:#}");
            EXIT_API
        }
    }
}

/// Prefer the stable symlink `~/.local/bin/vibeke` (hooks and holders reference it so upgrades
/// don't break them, 04 §11 rule 6) when it points at this binary.
fn stable_bin(bin: &Path) -> PathBuf {
    let stable = vk_server::paths::home().join(".local/bin/vibeke");
    match (std::fs::canonicalize(&stable), std::fs::canonicalize(bin)) {
        (Ok(a), Ok(b)) if a == b => stable,
        _ => bin.to_path_buf(),
    }
}

// ---- notify -----------------------------------------------------------------------------------

pub async fn notify(g: &Global, args: &[String]) -> i32 {
    let mut urgency = "normal".to_string();
    let mut pane: Option<String> = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--urgency" => {
                urgency = args.get(i + 1).cloned().unwrap_or_default();
                i += 1;
            }
            "--pane" => {
                pane = args.get(i + 1).cloned();
                i += 1;
            }
            "--current" => pane = Some("@current".into()),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    let Some(title) = rest.first().cloned() else {
        eprintln!("vibeke notify [--pane p|--current] [--urgency low|normal|high] <title> [body]");
        return EXIT_USAGE;
    };
    let pane = pane.or_else(|| {
        std::env::var("VIBEKE_PANE_TOKEN")
            .ok()
            .map(|_| "@current".to_string())
    });
    let params = json!({"title": title, "body": rest.get(1).cloned().unwrap_or_default(), "urgency": urgency, "pane": pane});
    crate::with_client(g, |mut c| async move {
        vk_cli::run_api(&mut c, g, "notification.send", params).await
    })
    .await
}

// ---- import herdr -------------------------------------------------------------------------------

pub async fn import(g: &Global, args: &[String]) -> i32 {
    if args.first().map(String::as_str) != Some("herdr") {
        eprintln!("vibeke import herdr [--config] [--session] [--dry-run] [--force]");
        return EXIT_USAGE;
    }
    let has = |f: &str| args.iter().any(|a| a == f);
    let (want_config, want_session) = match (has("--config"), has("--session")) {
        (false, false) => (true, true),
        x => x,
    };
    let dry = has("--dry-run");
    let herdr = vk_server::paths::home().join(".config/herdr");
    let mut code = EXIT_OK;
    if want_config {
        match std::fs::read_to_string(herdr.join("config.toml")) {
            Ok(src) => {
                let rep = vk_compat::import_config(&src);
                println!(
                    "# Herdr config import: {} mapped, {} unsupported",
                    rep.mapped.len(),
                    rep.unsupported.len()
                );
                for (a, b) in &rep.mapped {
                    println!("  mapped   {a} → {b}");
                }
                for u in &rep.unsupported {
                    println!("  skipped  {u}");
                }
                for w in &rep.warnings {
                    println!("  note     {w}");
                }
                if !dry {
                    let dir = vk_config::config_path()
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_default();
                    match vk_compat::write_imported(&dir, &rep.toml, has("--force")) {
                        Ok(p) => println!("wrote {}", p.display()),
                        Err(e) => {
                            eprintln!("write config: {e}");
                            code = EXIT_API;
                        }
                    }
                }
            }
            Err(e) => println!("no Herdr config ({e})"),
        }
    }
    if want_session {
        match std::fs::read_to_string(herdr.join("session.json")) {
            Ok(src) => match vk_compat::import_session(&src) {
                Ok(plan) => {
                    let resume = plan.resume_candidates();
                    println!(
                        "# Herdr session: {} workspaces, {} resumable agents",
                        plan.workspaces.len(),
                        resume.len()
                    );
                    if dry {
                        for w in &plan.workspaces {
                            println!("  workspace {} ({}) — {} tabs", w.name, w.cwd, w.tabs.len());
                        }
                        return code;
                    }
                    let r = crate::with_client(g, |mut c| async move {
                        recreate_session(&mut c, plan).await
                    })
                    .await;
                    if r != EXIT_OK {
                        code = r;
                    }
                }
                Err(e) => {
                    eprintln!("session.json: {e}");
                    code = EXIT_API;
                }
            },
            Err(e) => println!("no Herdr session ({e})"),
        }
    }
    code
}

/// Recreate Herdr workspaces/tabs/splits as Vibeke panes. Agents are offered for resume (the
/// command is printed and typed into the pane on request), never run automatically.
async fn recreate_session(c: &mut Client<crate::AnyStream>, plan: vk_compat::SessionPlan) -> i32 {
    use vk_compat::{Layout, Orientation};
    if c.hello("cli").await.is_err() {
        return EXIT_API;
    }
    let mut resumable: Vec<(String, String)> = Vec::new();
    for w in &plan.workspaces {
        let mut ws_id = String::new();
        for (ti, t) in w.tabs.iter().enumerate() {
            let leaf_cwd = t
                .layout
                .leaves()
                .first()
                .map(|l| l.cwd.clone())
                .unwrap_or_else(|| w.cwd.clone());
            let root = if ti == 0 {
                match c
                    .call("workspace.create", json!({"cwd": leaf_cwd, "name": w.name}))
                    .await
                {
                    Ok(v) => {
                        ws_id = v["workspace"]["id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        v["root_pane"]["id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string()
                    }
                    Err(e) => {
                        eprintln!("workspace {}: {e}", w.name);
                        break;
                    }
                }
            } else {
                match c
                    .call(
                        "tab.create",
                        json!({"workspace": ws_id, "cwd": leaf_cwd, "title": t.title}),
                    )
                    .await
                {
                    Ok(v) => v["root_pane"]["id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    Err(e) => {
                        eprintln!("tab: {e}");
                        continue;
                    }
                }
            };
            if ti == 0
                && let Some(title) = &t.title
            {
                let _ = c
                    .call("tab.rename", json!({"pane": root, "title": title}))
                    .await;
            }
            // Iterative layout walk: (layout, pane that holds this subtree).
            let mut stack: Vec<(&Layout, String)> = vec![(&t.layout, root)];
            while let Some((l, pane)) = stack.pop() {
                match l {
                    Layout::Leaf(leaf) => {
                        if let Some(argv) = leaf.agent.as_ref().and_then(|a| a.resume_argv()) {
                            resumable.push((pane, argv.join(" ")));
                        }
                    }
                    Layout::Split {
                        orientation,
                        ratio,
                        first,
                        second,
                    } => {
                        let dir = match orientation {
                            Orientation::SideBySide => "right",
                            Orientation::Stacked => "down",
                        };
                        let cwd = second.leaves().first().map(|l| l.cwd.clone());
                        let new = match c.call("pane.split", json!({"pane": pane, "direction": dir, "ratio": 1.0 - ratio, "cwd": cwd})).await {
                            Ok(v) => v["pane"]["id"].as_str().unwrap_or_default().to_string(),
                            Err(_) => continue,
                        };
                        stack.push((first, pane));
                        stack.push((second, new));
                    }
                }
            }
        }
        println!("  recreated {}", w.name);
    }
    if !resumable.is_empty() {
        println!("\nAgents to resume (`vibeke pane run <pane> '<cmd>'`):");
        for (pane, cmd) in resumable {
            println!("  {pane}: {cmd}");
        }
    }
    EXIT_OK
}

// ---- config / keys ------------------------------------------------------------------------------

pub fn config(g: &Global, args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("path") => {
            println!("{}", vk_config::config_path().display());
            EXIT_OK
        }
        Some("validate" | "check") => match vk_config::Config::load(vk_config::config_path()) {
            Ok((_, warnings)) => {
                for w in &warnings {
                    println!("warning: {w:?}");
                }
                println!("ok");
                EXIT_OK
            }
            Err(e) => {
                eprintln!("{e}");
                EXIT_API
            }
        },
        Some("default") => {
            print!("{}", vk_config::default_config_toml());
            EXIT_OK
        }
        Some("edit") => crate::config_cmd::edit(g, &args[1..]),
        Some("reset-keys") => crate::config_cmd::reset_keys(g, &args[1..]),
        _ => {
            eprintln!("vibeke config path|validate|default|edit|reset-keys");
            EXIT_USAGE
        }
    }
}

pub fn keys(_g: &Global, args: &[String]) -> i32 {
    let cfg = load_config();
    match args.first().map(String::as_str) {
        Some("check") => {
            let c = vk_config::check_keys(&cfg);
            for x in &c {
                println!("{x:?}");
            }
            if c.is_empty() { EXIT_OK } else { EXIT_API }
        }
        _ => {
            for (a, b) in &cfg.keys.bindings {
                if !b.is_empty() {
                    println!("{a:<28} {b}");
                }
            }
            EXIT_OK
        }
    }
}

// ---- later stages ----------------------------------------------------------------------------

pub fn hook(args: &[String]) -> i32 {
    vk_server::agents::hook::main(args)
}

pub async fn bridge(g: &Global, args: &[String]) -> i32 {
    crate::remote::bridge(g, args).await
}

pub async fn ssh(g: &Global, args: &[String]) -> i32 {
    crate::remote::ssh(g, args).await
}

pub async fn integration(g: &Global, args: &[String]) -> i32 {
    crate::integration::run(g, args).await
}

pub async fn doctor(g: &Global, args: &[String]) -> i32 {
    crate::doctor::run(g, args).await
}

pub async fn update(g: &Global, args: &[String]) -> i32 {
    crate::doctor::update(g, args).await
}
