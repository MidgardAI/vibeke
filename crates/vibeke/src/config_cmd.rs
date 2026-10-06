//! `vibeke config edit` and `vibeke config reset-keys` (07 §5.3, 08 §11): local commands on
//! `config.toml`, no server needed (a running server picks the change up through its config
//! watcher, or `vibeke config reload`).
//!
//! - `edit [--no-reopen]` opens `$VISUAL`, else `$EDITOR`, else `vi` on `config.toml` (created
//!   from the commented default file when missing), then validates the saved file. On errors
//!   it prints them located (`file:line:col`) and, on a terminal, offers to reopen the editor;
//!   declining keeps the file as saved (a running server keeps its applied config until the
//!   file is valid). Exit 0 when the file is valid, 1 otherwise.
//! - `reset-keys [--all] [--yes] [--dry-run]` removes every `[keys]` setting so the default
//!   keymap applies again. `[[keys.command]]` entries are kept unless `--all`. The previous
//!   file is saved as `config.toml.<unix-time>.bak` first. Without a terminal it needs `--yes`.

use serde_json::json;
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use vk_cli::{EXIT_API, EXIT_OK, EXIT_USAGE, Global};

fn as_json(g: &Global) -> bool {
    g.json.unwrap_or(!std::io::stdout().is_terminal())
}

fn ask(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    eprint!("{question} [Y/n] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    !matches!(line.trim().to_lowercase().as_str(), "n" | "no")
}

/// The editor command: `$VISUAL`, `$EDITOR`, else `vi`, split on whitespace (`code -w`).
fn editor() -> Vec<String> {
    let raw = std::env::var("VISUAL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".into());
    raw.split_whitespace().map(str::to_string).collect()
}

fn validate(path: &Path) -> Result<Vec<vk_config::Warning>, vk_config::ConfigError> {
    let src = std::fs::read_to_string(path).map_err(|e| vk_config::ConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;
    vk_config::Config::parse(&src, path).map(|(_, w)| w)
}

/// `vibeke config edit`.
pub fn edit(g: &Global, args: &[String]) -> i32 {
    let no_reopen = args.iter().any(|a| a == "--no-reopen");
    let path = vk_config::config_path();
    if !path.exists()
        && let Err(e) = vk_config::edit::write_atomic(&path, &vk_config::default_config_toml())
    {
        eprintln!("create {}: {e}", path.display());
        return EXIT_API;
    }
    let cmd = editor();
    loop {
        let status = std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .arg(&path)
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                eprintln!("editor `{}` exited with {s}", cmd.join(" "));
                return EXIT_API;
            }
            Err(e) => {
                eprintln!("cannot run editor `{}`: {e}", cmd.join(" "));
                return EXIT_API;
            }
        }
        match validate(&path) {
            Ok(warnings) => {
                if as_json(g) {
                    println!(
                        "{}",
                        json!({"path": path, "valid": true, "warnings": warnings.iter().map(|w| w.to_string()).collect::<Vec<_>>()})
                    );
                } else {
                    for w in &warnings {
                        eprintln!("warning: {w}");
                    }
                    println!("{} is valid", path.display());
                }
                return EXIT_OK;
            }
            Err(e) => {
                eprintln!("{e}");
                if !no_reopen
                    && std::io::stderr().is_terminal()
                    && ask("Reopen the editor to fix it?")
                {
                    continue;
                }
                if as_json(g) {
                    println!(
                        "{}",
                        json!({"path": path, "valid": false, "errors": [e.to_string()]})
                    );
                }
                eprintln!(
                    "kept {} as saved; a running server keeps its current config until the file is valid",
                    path.display()
                );
                return EXIT_API;
            }
        }
    }
}

/// `vibeke config reset-keys`.
pub fn reset_keys(g: &Global, args: &[String]) -> i32 {
    let all = args.iter().any(|a| a == "--all");
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let dry = args.iter().any(|a| a == "--dry-run");
    if let Some(bad) = args
        .iter()
        .find(|a| !matches!(a.as_str(), "--all" | "--yes" | "-y" | "--dry-run"))
    {
        eprintln!("unknown flag {bad}\nvibeke config reset-keys [--all] [--yes] [--dry-run]");
        return EXIT_USAGE;
    }
    let path = vk_config::config_path();
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            return EXIT_API;
        }
    };
    let (text, removed) = match vk_config::layers::reset_keys_text(&src, all) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{}: {e}", path.display());
            return EXIT_API;
        }
    };
    let report = |backup: Option<&Path>, applied: bool| {
        if as_json(g) {
            println!(
                "{}",
                json!({"path": path, "removed": removed, "backup": backup, "applied": applied, "dry_run": dry})
            );
        } else if removed.is_empty() {
            println!(
                "no key settings in {}; the default keymap applies",
                path.display()
            );
        } else {
            for k in &removed {
                println!("{} {k}", if applied { "removed" } else { "would remove" });
            }
            if let Some(b) = backup {
                println!("previous file saved as {}", b.display());
            }
        }
    };
    if removed.is_empty() || dry {
        report(None, false);
        return EXIT_OK;
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            eprintln!("reset-keys rewrites {}; rerun with --yes", path.display());
            return EXIT_USAGE;
        }
        for k in &removed {
            eprintln!("  {k}");
        }
        if !ask(&format!("Remove these from {}?", path.display())) {
            return EXIT_API;
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let backup = path.with_file_name(format!(
        "{}.{secs}.bak",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.toml")
    ));
    if let Err(e) = vk_config::edit::write_atomic(&backup, &src) {
        eprintln!("backup {}: {e}", backup.display());
        return EXIT_API;
    }
    if let Err(e) = vk_config::edit::write_atomic(&path, &text) {
        eprintln!("write {}: {e}", path.display());
        return EXIT_API;
    }
    report(Some(&backup), true);
    EXIT_OK
}
