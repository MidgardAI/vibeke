//! `vibeke setup` (08 §9) and `vibeke trust` (08 §11.1), the CLI side of onboarding and repo
//! trust. Both reuse the TUI's onboarding code (`vk_tui::onboarding`), so the steps and the
//! written config are the same.
//!
//! `vibeke setup` asks on stdin unless `--yes`; nothing touches a harness config without a yes
//! (the exact diff is printed first), and `--dry-run` writes nothing at all. Consent is never
//! assumed: without a terminal and without `--yes` nothing is written, and end of input or a
//! read error at any question means "no" and stops setup (never a prompt's default "yes").
//! Non-interactive use:
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

/// Read one answer line; `None` at end of input or on a read error (never an empty answer, so
/// a closed stdin can't select a prompt's default).
fn ask_from(input: &mut dyn BufRead, prompt: &str) -> Option<String> {
    print!("{prompt} ");
    let _ = std::io::stdout().flush();
    let mut s = String::new();
    match input.read_line(&mut s) {
        Ok(0) | Err(_) => {
            println!();
            None
        }
        Ok(_) => Some(s.trim().to_string()),
    }
}

/// [`ask_from`] on stdin.
fn ask(prompt: &str) -> Option<String> {
    ask_from(&mut std::io::stdin().lock(), prompt)
}

/// The final "Write <config>?" question: `Some(answer)` (enter keeps the default yes, an
/// explicit answer), `None` at end of input or on a read error, which never writes.
fn confirm_write(input: &mut dyn BufRead, path: &std::path::Path) -> Option<bool> {
    ask_from(input, &format!("Write {}? [Y/n]", path.display())).map(|a| yes(&a, true))
}

/// Setup stopped because input ended: what was answered stays, nothing more is written.
fn input_closed() -> i32 {
    eprintln!("vibeke setup: input closed; stopped without writing anything further");
    EXIT_USAGE
}

fn yes(answer: &str, default: bool) -> bool {
    match answer.to_lowercase().as_str() {
        "" => default,
        a => a.starts_with('y'),
    }
}

pub fn setup(args: &[String]) -> i32 {
    let tty = std::io::stdin().is_terminal();
    setup_with(
        args,
        &mut std::io::stdin().lock(),
        tty,
        &vk_config::config_path(),
    )
}

/// `vibeke setup` reading answers from `input` (`tty`: it is a terminal) and writing `path`.
pub fn setup_with(
    args: &[String],
    input: &mut dyn BufRead,
    tty: bool,
    path: &std::path::Path,
) -> i32 {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{SETUP_USAGE}");
        return EXIT_OK;
    }
    let auto = args.iter().any(|a| a == "--yes" || a == "-y");
    let dry = args.iter().any(|a| a == "--dry-run");
    if !auto && !dry && !tty {
        eprintln!(
            "vibeke setup: stdin is not a terminal and --yes was not given; nothing written. Run it in a terminal, or non-interactively: {SETUP_USAGE}"
        );
        return EXIT_USAGE;
    }
    let interactive = !auto && tty;
    // 1. Terminal.
    println!("== 1. terminal ==");
    if tty && std::io::stdout().is_terminal() {
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
            let Some(a) = ask_from(
                input,
                "Import keybindings, theme, sidebar rules and worktree dir? [Y/n]",
            ) else {
                return input_closed();
            };
            yes(&a, true)
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
            let Some(a) = ask_from(
                input,
                &format!(
                    "Install the {} integration into {}? [y/N]",
                    r.harness.id(),
                    files.join(", ")
                ),
            ) else {
                return input_closed();
            };
            yes(&a, false)
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
            let Some(a) = ask_from(
                input,
                &format!(
                    "[1] native ({native})  [2] terminal (OSC 9/777)  [3] none — enter keeps native:"
                ),
            ) else {
                return input_closed();
            };
            match a.as_str() {
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
            let Some(a) = ask_from(input, &format!("theme ({list}) — enter keeps catppuccin:"))
            else {
                return input_closed();
            };
            if a.is_empty() { "catppuccin".into() } else { a }
        }
        None => "catppuccin".into(),
    };
    println!("  {theme}");
    choices.theme = Some(theme);
    // 6. Config.
    println!("\n== 6. config: {} ==", path.display());
    let existing = std::fs::read_to_string(path).ok();
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
    let write = if interactive {
        match confirm_write(input, path) {
            Some(w) => w,
            None => return input_closed(),
        }
    } else {
        auto
    };
    if write {
        match ob::write_config(path, &text) {
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
            && !ask("Trust this content (any later change needs a new review)? [y/N]")
                .is_some_and(|a| yes(&a, false))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// Review batch 2, finding 12: closed stdin and EOF at the final prompt leave both an
    /// absent and an existing configuration untouched; without a terminal and without
    /// `--yes` nothing is written. (No scripted answer here can reach a harness install
    /// question: closed input stops at the first question, whatever it is.)
    #[test]
    fn eof_and_closed_stdin_never_write_the_config() {
        let d = tempfile::tempdir().unwrap();
        let absent = d.path().join("absent.toml");
        let existing = d.path().join("existing.toml");
        let mine = "theme = \"gruvbox\"\n[notifications]\nchannel = \"osc\"\n";
        std::fs::write(&existing, mine).unwrap();
        for path in [&absent, &existing] {
            // Closed stdin, with and without a terminal.
            for tty in [true, false] {
                let code = setup_with(&args(&["setup"]), &mut std::io::empty(), tty, path);
                assert_eq!(code, EXIT_USAGE, "{} tty={tty}", path.display());
            }
            // Piped answers without a terminal: refused before any question.
            let mut answers: &[u8] = b"3\n\ny\n";
            let code = setup_with(&args(&["setup"]), &mut answers, false, path);
            assert_eq!(code, EXIT_USAGE);
        }
        assert!(!absent.exists(), "no config was created");
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), mine);
        // EOF (or a read error) at the final question is "no", never the default yes.
        assert_eq!(confirm_write(&mut std::io::empty(), &absent), None);
        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("tty gone"))
            }
        }
        let mut broken = std::io::BufReader::new(Broken);
        assert_eq!(confirm_write(&mut broken, &absent), None);
        let mut n: &[u8] = b"n\n";
        assert_eq!(confirm_write(&mut n, &absent), Some(false));
        let mut enter: &[u8] = b"\n";
        assert_eq!(
            confirm_write(&mut enter, &absent),
            Some(true),
            "an explicit enter"
        );
        // --yes writes without a terminal (no harness is named, so none is installed).
        let auto = d.path().join("auto.toml");
        let code = setup_with(
            &args(&["setup", "--yes"]),
            &mut std::io::empty(),
            false,
            &auto,
        );
        assert_eq!(code, EXIT_OK);
        assert!(auto.exists());
    }
}
