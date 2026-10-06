//! `vibeke integration install|status|uninstall|doctor <claude|codex|pi|omp|all>` (04 §11).
//!
//! Writing the user's real `~/.claude` / `~/.codex` needs explicit consent: without `--yes` the
//! command shows the planned diff only. `CLAUDE_CONFIG_DIR` / `CODEX_HOME` redirect it (e.g. to
//! temporary copies).

use vk_agents::{Dirs, Harness, InstallState};
use vk_cli::{EXIT_API, EXIT_OK, EXIT_USAGE, Global};

fn harnesses(arg: Option<&str>) -> Option<Vec<Harness>> {
    match arg {
        Some("claude") => Some(vec![Harness::Claude]),
        Some("codex") => Some(vec![Harness::Codex]),
        Some("pi") => Some(vec![Harness::Pi]),
        Some("omp") => Some(vec![Harness::Omp]),
        Some("all") | None => Some(vec![
            Harness::Claude,
            Harness::Codex,
            Harness::Pi,
            Harness::Omp,
        ]),
        _ => None,
    }
}

fn stable_bin() -> std::path::PathBuf {
    let stable = vk_server::paths::home().join(".local/bin/vibeke");
    if stable.exists() {
        stable
    } else {
        std::env::current_exe().unwrap_or(stable)
    }
}

pub async fn run(_g: &Global, args: &[String]) -> i32 {
    let verb = args.first().map(String::as_str).unwrap_or("status");
    let target = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .map(String::as_str);
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let dry = args.iter().any(|a| a == "--dry-run");
    let Some(hs) = harnesses(target) else {
        eprintln!("vibeke integration {verb} <claude|codex|pi|omp|all> [--dry-run] [--yes]");
        return EXIT_USAGE;
    };
    let dirs = Dirs::from_env();
    let redirected = [
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "PI_CODING_AGENT_DIR",
        "VIBEKE_PI_HOME",
        "VIBEKE_OMP_HOME",
    ]
    .iter()
    .any(|k| std::env::var_os(k).is_some());
    match verb {
        "list" | "status" => {
            for h in hs {
                let st = vk_agents::status(h, &dirs);
                let state = match st.state {
                    InstallState::Installed => "installed",
                    InstallState::Partial => "partial",
                    InstallState::NotInstalled => "not installed",
                };
                println!("{:<7} {state:<14} {}", h.id(), st.file.display());
                for m in &st.missing_events {
                    println!("        missing: {m}");
                }
                for f in &st.foreign {
                    println!("        foreign (left alone): {f}");
                }
                for t in &st.todo {
                    println!("        todo: {t}");
                }
                for p in &st.problems {
                    println!("        problem: {p}");
                }
                if h == Harness::Codex {
                    let untrusted = st
                        .hooks
                        .iter()
                        .filter(|x| x.trust == Some(vk_agents::Trust::Untrusted))
                        .count();
                    if untrusted > 0 {
                        println!(
                            "        {untrusted} hook(s) untrusted — run /hooks in Codex once"
                        );
                    }
                }
            }
            EXIT_OK
        }
        "install" | "uninstall" => {
            let mut code = EXIT_OK;
            for h in hs {
                let plan = if verb == "install" {
                    vk_agents::plan_install(h, &dirs, &stable_bin())
                } else {
                    vk_agents::plan_uninstall(h, &dirs)
                };
                let plan = match plan {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("{}: {e:#}", h.id());
                        code = EXIT_API;
                        continue;
                    }
                };
                if !plan.changed() {
                    println!(
                        "{}: already {}",
                        h.id(),
                        if verb == "install" {
                            "installed"
                        } else {
                            "removed"
                        }
                    );
                    continue;
                }
                for f in plan.files.iter().filter(|f| f.changed()) {
                    println!("--- {} ({})", f.path.display(), h.id());
                    println!("{}", f.diff());
                }
                for n in &plan.notes {
                    println!("note: {n}");
                }
                if dry || (!yes && !redirected) {
                    println!(
                        "{}: dry run — rerun with --yes to write {}",
                        h.id(),
                        dirs.config_file(h).display()
                    );
                    continue;
                }
                match vk_agents::apply(&plan) {
                    Ok(paths) => {
                        for p in paths {
                            println!("wrote {}", p.display());
                        }
                    }
                    Err(e) => {
                        eprintln!("{}: {e:#}", h.id());
                        code = EXIT_API;
                    }
                }
            }
            if verb == "install" && hs_contains_codex(target) {
                let shims = vk_server::paths::Paths::shims();
                match vk_agents::write_codex_shim(&shims) {
                    Ok(p) => println!("codex PATH shim: {}", p.display()),
                    Err(e) => eprintln!("codex shim: {e:#}"),
                }
            }
            code
        }
        "doctor" => {
            for h in hs {
                let st = vk_agents::status(h, &dirs);
                let version = std::process::Command::new(h.id())
                    .arg("--version")
                    .output()
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
                println!(
                    "{}: binary {} · hooks {:?} · {}",
                    h.id(),
                    version.as_deref().unwrap_or("not found"),
                    st.state,
                    st.file.display()
                );
                if std::env::var("VIBEKE").as_deref() != Ok("1") {
                    println!("  (run inside a vibeke pane to verify hooks reach the server)");
                }
            }
            EXIT_OK
        }
        _ => {
            eprintln!(
                "vibeke integration list|status|install|uninstall|doctor <claude|codex|pi|omp|all>"
            );
            EXIT_USAGE
        }
    }
}

fn hs_contains_codex(target: Option<&str>) -> bool {
    matches!(target, None | Some("all") | Some("codex"))
}
