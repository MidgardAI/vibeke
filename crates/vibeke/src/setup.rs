//! `vibeke setup` (08 §9) and `vibeke trust` (08 §11.1), the CLI side of onboarding and repo
//! trust. Both reuse the TUI's onboarding code (`vk_tui::onboarding`), so the steps and the
//! written config are the same.
//!
//! `vibeke setup` asks on stdin unless `--yes`; nothing touches a harness config without a yes
//! (the exact diff is printed first), and `--dry-run` writes nothing at all. Non-interactive use:
//! `--install claude,codex|all --notifications native|osc|none --theme NAME --import-herdr --yes`.

use std::io::{BufRead, IsTerminal, Write};
use vk_cli::client::Client;
use vk_cli::{EXIT_API, EXIT_OK, EXIT_USAGE, Global};
use vk_tui::onboarding as ob;

pub const SETUP_USAGE: &str = "vibeke setup [--yes] [--dry-run] [--install claude,codex|all] [--notifications native|osc|none] [--theme NAME] [--import-herdr]";
pub const TRUST_USAGE: &str = "vibeke trust [path] [--yes] [--check]";

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// Read one answer line (EOF = empty).
fn ask(prompt: &str) -> String {
    print!("{prompt} ");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    let _ = std::io::stdin().lock().read_line(&mut s);
    s.trim().to_string()
}

fn yes(answer: &str, default: bool) -> bool {
    match answer.to_lowercase().as_str() {
        "" => default,
        a => a.starts_with('y'),
    }
}

pub fn setup(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{SETUP_USAGE}");
        return EXIT_OK;
    }
    let auto = args.iter().any(|a| a == "--yes" || a == "-y");
    let dry = args.iter().any(|a| a == "--dry-run");
    let interactive = !auto;
    // 1. Terminal.
    println!("== 1. terminal ==");
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        match crate::doctor::probe_terminal() {
            Some(p) => {
                let caps = vk_tui::screen::HostCaps {
                    truecolor: p.truecolor,
                    sync_update: p.sync_update,
                    notifications: p.notifications,
                    ..Default::default()
                };
                let osc52 = matches!(p.osc52, vk_tui::caps::Osc52::Allowed);
                for c in ob::terminal_checks(&caps, p.kitty_keyboard, osc52) {
                    println!(
                        "  {}  {:<14} {}",
                        if c.ok { "✓ pass" } else { "! warn" },
                        c.name,
                        c.detail
                    );
                }
            }
            None => println!("  (no answer from the terminal; vibeke doctor has the details)"),
        }
    } else {
        println!(
            "  (not a terminal: run `vibeke setup` in your terminal for the capability table)"
        );
    }
    let mut choices = ob::Choices::default();
    // 2. Herdr.
    if let Some(h) = ob::herdr_config() {
        println!("\n== 2. Herdr ==\nfound {}", h.display());
        let import = if interactive {
            yes(
                &ask("Import keybindings, theme, sidebar rules and worktree dir? [Y/n]"),
                true,
            )
        } else {
            args.iter().any(|a| a == "--import-herdr")
        };
        if import && let Ok(src) = std::fs::read_to_string(&h) {
            let rep = vk_compat::import_config(&src);
            println!(
                "  importing {} setting(s), {} unsupported",
                rep.mapped.len(),
                rep.unsupported.len()
            );
            choices.herdr_toml = Some(rep.toml);
        }
    }
    // 3. Integrations.
    println!("\n== 3. agent integrations ==");
    let dirs = vk_agents::Dirs::from_env();
    let bin = ob::vibeke_bin();
    let rows = ob::detect_harnesses(&dirs, &bin);
    for r in &rows {
        println!(
            "  {:<9} {:<14} {}",
            r.harness.id(),
            r.state_text(),
            r.binary
                .as_ref()
                .map(|b| b.display().to_string())
                .unwrap_or_else(|| "not on PATH".into())
        );
    }
    let wanted: Vec<String> = flag(args, "--install")
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
        .unwrap_or_default();
    let mut code = EXIT_OK;
    for r in &rows {
        if r.files.is_empty() {
            continue;
        }
        let listed = wanted.iter().any(|w| w == "all" || w == r.harness.id());
        if !interactive && !listed {
            continue;
        }
        if interactive && r.binary.is_none() && !listed {
            continue;
        }
        println!("\n{}", r.diff);
        let files: Vec<String> = r.files.iter().map(|p| p.display().to_string()).collect();
        let go = if interactive {
            yes(
                &ask(&format!(
                    "Install the {} integration into {}? [y/N]",
                    r.harness.id(),
                    files.join(", ")
                )),
                false,
            )
        } else {
            true
        };
        if !go {
            continue;
        }
        if dry {
            println!("  dry run: {} not written", files.join(", "));
            continue;
        }
        match ob::install(r.harness, &dirs, &bin) {
            Ok(paths) => {
                for p in paths {
                    println!("  wrote {}", p.display());
                }
            }
            Err(e) => {
                eprintln!("  {}: {e}", r.harness.id());
                code = EXIT_API;
            }
        }
    }
    // 4. Notifications.
    println!("\n== 4. notifications ==");
    let notif = match flag(args, "--notifications") {
        Some(n) if ["native", "osc", "none"].contains(&n.as_str()) => n,
        Some(n) => {
            eprintln!("--notifications {n}: native | osc | none");
            return EXIT_USAGE;
        }
        None if interactive => {
            let native = ob::native_notifier().unwrap_or("none found");
            match ask(&format!(
                "[1] native ({native})  [2] terminal (OSC 9/777)  [3] none — enter keeps native:"
            ))
            .as_str()
            {
                "2" | "osc" | "terminal" => "osc".into(),
                "3" | "none" => "none".into(),
                _ => "native".into(),
            }
        }
        None => "native".into(),
    };
    println!("  {notif}");
    choices.notifications = Some(notif);
    // 5. Theme.
    println!("\n== 5. theme ==");
    let theme = match flag(args, "--theme") {
        Some(t) => t,
        None if interactive => {
            let list = ob::THEMES.join(" | ");
            let a = ask(&format!("theme ({list}) — enter keeps catppuccin:"));
            if a.is_empty() { "catppuccin".into() } else { a }
        }
        None => "catppuccin".into(),
    };
    println!("  {theme}");
    choices.theme = Some(theme);
    // 6. Config.
    let path = vk_config::config_path();
    println!("\n== 6. config: {} ==", path.display());
    let existing = std::fs::read_to_string(&path).ok();
    let text = match ob::starter_config(existing.as_deref(), &choices) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("can't build the config: {e}");
            return EXIT_API;
        }
    };
    print!("{text}");
    if dry {
        println!("dry run: not written");
        return code;
    }
    let write = !interactive || yes(&ask(&format!("Write {}? [Y/n]", path.display())), true);
    if write {
        match ob::write_config(&path, &text) {
            Ok(()) => println!("wrote {}", path.display()),
            Err(e) => {
                eprintln!("write {}: {e}", path.display());
                return EXIT_API;
            }
        }
    }
    code
}

/// `vibeke trust [path] [--yes] [--check]`: review a repo's `.vibeke/` and record trust.
pub async fn trust(g: &Global, args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{TRUST_USAGE}");
        return EXIT_OK;
    }
    let path = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .cloned()
        .unwrap_or_else(|| ".".into());
    let path = std::fs::canonicalize(&path)
        .map(|p| p.display().to_string())
        .unwrap_or(path);
    let auto = args.iter().any(|a| a == "--yes" || a == "-y");
    let only_check = args.iter().any(|a| a == "--check");
    crate::with_client(g, |mut c: Client<crate::AnyStream>| async move {
        if let Err(e) = c.hello("cli").await {
            eprintln!("{e}");
            return EXIT_API;
        }
        let info = match c
            .call(
                "policy.trust",
                serde_json::json!({"path": path, "check": true}),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                return EXIT_API;
            }
        };
        let repo = info["repo"].as_str().unwrap_or_default().to_string();
        let digest = info["digest"].as_str().map(str::to_string);
        println!("repo:    {repo}");
        match info["file"].as_str() {
            Some(f) => println!("file:    {f}"),
            None => println!("file:    (no .vibeke/config.toml)"),
        }
        println!("digest:  {}", digest.as_deref().unwrap_or("(no .vibeke/)"));
        let trusted = info["trusted"].as_bool().unwrap_or(false);
        println!("trusted: {}", if trusted { "yes" } else { "no" });
        if let Some(e) = info["error"].as_str() {
            println!("error:   {e}");
        }
        for w in info["warnings"].as_array().into_iter().flatten() {
            println!("warning: {}", w.as_str().unwrap_or_default());
        }
        if let Some(t) = info["text"].as_str() {
            println!("--- .vibeke/config.toml ---\n{t}---");
        }
        if only_check || trusted {
            return EXIT_OK;
        }
        if digest.is_none() {
            eprintln!("nothing to trust: {repo} has no .vibeke/ directory");
            return EXIT_USAGE;
        }
        if !auto
            && !yes(
                &ask("Trust this content (any later change needs a new review)? [y/N]"),
                false,
            )
        {
            println!("not trusted");
            return EXIT_OK;
        }
        match c
            .call(
                "policy.trust",
                serde_json::json!({"path": repo, "digest": digest}),
            )
            .await
        {
            Ok(v) => {
                println!("trusted {repo} ({})", v["digest"].as_str().unwrap_or("-"));
                if let Some(s) = v["setup_script"].as_str() {
                    println!("--- .vibeke/setup.sh (runs in new task worktrees) ---\n{s}---");
                }
                EXIT_OK
            }
            Err(e) => {
                eprintln!("{e}");
                EXIT_API
            }
        }
    })
    .await
}
