//! `vibeke integration list|install|status|uninstall|doctor|capabilities|update` (04 §11, §13).
//!
//! Writing the user's real harness configs needs explicit consent: without `--yes` the command
//! shows the planned diff only. `CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `VIBEKE_PI_HOME` /
//! `VIBEKE_OMP_HOME` / `VIBEKE_OPENCODE_HOME` / `VIBEKE_GEMINI_HOME` redirect it (e.g. to
//! temporary copies).
//!
//! `doctor` and `capabilities` read the harness manifests (built-ins, the verified remote cache
//! and `<config dir>/harnesses/*.toml`) and report each installed harness's version against the
//! manifest's validated range: outside it, runs get `observe` (+ keystrokes) only (04 §12.3).
//! `--mcp` installs/removes/reports the `vibeke mcp` server entry instead of the hooks (06 B7):
//! Claude `mcpServers` in `.claude.json`, Codex `[mcp_servers.vibeke]` in `config.toml`.

use serde_json::{Value, json};
use vk_agents::manifest::{self, Loaded, Sources};
use vk_agents::{Dirs, Harness, InstallState};
use vk_cli::{EXIT_API, EXIT_OK, EXIT_USAGE, Global};

const USAGE: &str = "vibeke integration list|status|install|uninstall|doctor|capabilities <claude|codex|pi|omp|opencode|gemini|all> [--dry-run] [--yes]\n       vibeke integration list --sources         (where each manifest came from: built-in, remote channel, user, repo)\n       vibeke integration update [--url URL]   (signed manifest channel; refuses unsigned indexes)\n       vibeke integration pin <id> [<version>|current]   (freeze a cached remote manifest)\n       vibeke integration unpin <id>";

fn harnesses(arg: Option<&str>) -> Option<Vec<Harness>> {
    match arg {
        Some("all") | None => Some(Harness::ALL.to_vec()),
        Some(id) => Harness::from_id(id).map(|h| vec![h]),
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

fn manifests() -> manifest::Set {
    manifest::load(&Sources {
        user_dir: Some(vk_server::agents::manifests::user_dir()),
        remote: vk_server::agents::channel::cached_dir(),
        trusted_repos: vec![],
    })
}

/// Run a manifest's `[version] command` (only when the binary exists on PATH).
fn probe_version(l: &Loaded) -> Option<(String, String)> {
    let cmd = &l.m.version.command;
    let bin = cmd.first()?;
    let found = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(bin))
            .find(|c| c.is_file())
    })?;
    let out = std::process::Command::new(&found)
        .args(&cmd[1..])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let v = l.parse_version(&text)?;
    Some((found.to_string_lossy().into_owned(), v))
}

fn doctor_row(l: &Loaded) -> Value {
    let probe = probe_version(l);
    let range = l.validated_range();
    let (status, caps) = match &probe {
        None => ("not_installed", l.capabilities(None, "tui")),
        Some((_, v)) if l.validated(v) => ("validated", l.capabilities(Some(v), "tui")),
        Some(_) => {
            let mut c = vec!["observe".to_string()];
            if l.has_dialog_rules() {
                c.push("answer_keystroke".into());
            }
            ("unvalidated", c)
        }
    };
    json!({
        "id": l.m.id,
        "name": l.display(),
        "source": l.source.label(),
        "binary": probe.as_ref().map(|p| p.0.clone()),
        "version": probe.as_ref().map(|p| p.1.clone()),
        "validated_range": range,
        "status": status,
        "capabilities": caps,
        "unverified": l.unverified_capabilities(),
    })
}

fn is_json(g: &Global) -> bool {
    use std::io::IsTerminal;
    g.json.unwrap_or(!std::io::stdout().is_terminal())
}

pub async fn run(g: &Global, args: &[String]) -> i32 {
    let verb = args.first().map(String::as_str).unwrap_or("status");
    let target = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .map(String::as_str);
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let dry = args.iter().any(|a| a == "--dry-run");
    match verb {
        "doctor" | "capabilities" => return doctor(g, verb, target),
        "update" => return update(g, args),
        "list" if args.iter().any(|a| a == "--sources") => return sources(g),
        "pin" | "unpin" => return pin(g, verb == "unpin", args),
        _ => {}
    }
    let Some(hs) = harnesses(target) else {
        eprintln!("{USAGE}");
        return EXIT_USAGE;
    };
    let dirs = Dirs::from_env();
    let redirected = Dirs::REDIRECT_VARS
        .iter()
        .any(|k| std::env::var_os(k).is_some());
    let mcp = args.iter().any(|a| a == "--mcp");
    if mcp {
        return run_mcp(verb, hs, &dirs, yes || redirected, dry);
    }
    match verb {
        "list" | "status" => {
            for h in hs {
                let st = vk_agents::status(h, &dirs);
                let state = match st.state {
                    InstallState::Installed => "installed",
                    InstallState::Partial => "partial",
                    InstallState::NotInstalled => "not installed",
                };
                println!("{:<8} {state:<14} {}", h.id(), st.file.display());
                for m in &st.missing_events {
                    println!("         missing: {m}");
                }
                for f in &st.foreign {
                    println!("         foreign (left alone): {f}");
                }
                for t in &st.todo {
                    println!("         todo: {t}");
                }
                for p in &st.problems {
                    println!("         problem: {p}");
                }
                if h == Harness::Codex {
                    let untrusted = st
                        .hooks
                        .iter()
                        .filter(|x| x.trust == Some(vk_agents::Trust::Untrusted))
                        .count();
                    if untrusted > 0 {
                        println!(
                            "         {untrusted} hook(s) untrusted — run /hooks in Codex once"
                        );
                    }
                }
            }
            if verb == "list" && target.is_none() {
                let set = manifests();
                println!("\nmanifests (04 §5):");
                for l in &set.manifests {
                    println!(
                        "  {:<14} {:<8} {}{}",
                        l.m.id,
                        l.source.label(),
                        l.display(),
                        if l.m.detect.process.is_empty() {
                            " (not detected; referenced/screen-only)"
                        } else {
                            ""
                        }
                    );
                }
                for w in &set.warnings {
                    println!("  warning: {w}");
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
                    if !dry {
                        record_integrity(verb, h, &dirs);
                    }
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
                        // 09 §11: integration installs are audited (no server needed).
                        vk_server::audit::record_offline(
                            &g.session,
                            &format!("integration.{verb}ed"),
                            json!({"harness": h.id()}),
                            json!({"files": paths}),
                        );
                        for p in paths {
                            println!("wrote {}", p.display());
                        }
                        record_integrity(verb, h, &dirs);
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
        _ => {
            eprintln!("{USAGE}");
            EXIT_USAGE
        }
    }
}

fn doctor(g: &Global, verb: &str, target: Option<&str>) -> i32 {
    let set = manifests();
    let rows: Vec<&Loaded> = set
        .manifests
        .iter()
        .filter(|l| match target {
            None | Some("all") => !l.m.detect.process.is_empty(),
            Some(id) => l.m.id == id,
        })
        .collect();
    if rows.is_empty() {
        eprintln!("unknown harness {}", target.unwrap_or(""));
        return EXIT_USAGE;
    }
    let dirs = Dirs::from_env();
    let report: Vec<Value> = rows
        .iter()
        .map(|l| {
            let mut r = doctor_row(l);
            if let Some(h) = Harness::from_id(&l.m.id) {
                let st = vk_agents::status(h, &dirs);
                r["integration"] = json!({
                    "file": st.file,
                    "state": format!("{:?}", st.state).to_lowercase(),
                    "todo": st.todo,
                    "problems": st.problems,
                });
            }
            r
        })
        .collect();
    if is_json(g) {
        println!(
            "{}",
            json!({"harnesses": report, "warnings": set.warnings, "inside_vibeke": std::env::var("VIBEKE").as_deref() == Ok("1")})
        );
        return EXIT_OK;
    }
    for r in &report {
        let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
        let version = r["version"].as_str().unwrap_or("not found");
        let range = r["validated_range"]
            .as_str()
            .unwrap_or("none (unverified: observe + keystrokes)");
        println!(
            "{:<10} {:<9} version {version} · validated {range} · {}",
            s("id"),
            s("source"),
            match s("status").as_str() {
                "validated" => "ok".to_string(),
                "unvalidated" =>
                    "UNVALIDATED → observe-only until the golden corpus covers it".into(),
                _ => "not installed".into(),
            }
        );
        if verb == "capabilities" || s("status") != "not_installed" {
            let caps: Vec<String> =
                serde_json::from_value(r["capabilities"].clone()).unwrap_or_default();
            println!("           capabilities: {}", caps.join(", "));
            let unv: Vec<String> =
                serde_json::from_value(r["unverified"].clone()).unwrap_or_default();
            if !unv.is_empty() {
                println!("           documented, unverified (✓?): {}", unv.join(", "));
            }
        }
        if let Some(i) = r.get("integration") {
            println!(
                "           integration: {} ({})",
                i["state"].as_str().unwrap_or(""),
                i["file"].as_str().unwrap_or("")
            );
        }
    }
    for w in &set.warnings {
        println!("warning: {w}");
    }
    if std::env::var("VIBEKE").as_deref() != Ok("1") {
        println!("(run inside a vibeke pane to verify hooks reach the server)");
    }
    EXIT_OK
}

/// `list --sources` (04 §13): provenance of every manifest: the layers it was built from, the
/// remote channel's serial/version/verification, and pins.
fn sources(g: &Global) -> i32 {
    use vk_server::agents::channel;
    let set = manifests();
    let root = channel::root();
    let state = channel::read_state(&root);
    let pins = channel::read_pins(&root);
    let rows: Vec<Value> = set
        .manifests
        .iter()
        .map(|l| {
            let id = l.m.id.clone();
            let remote = state.as_ref().and_then(|s| s.sources.get(&id));
            let mut layers = vec!["builtin".to_string()];
            if let Some(r) = remote {
                layers.push(format!("remote@{}", r.serial));
            }
            let file = match &l.source {
                manifest::Source::User(p) => {
                    layers.push("user".into());
                    Some(p.display().to_string())
                }
                manifest::Source::Repo { file, .. } => {
                    layers = vec![format!("repo:{}", file.display())];
                    Some(file.display().to_string())
                }
                _ => None,
            };
            json!({
                "id": id,
                "source": l.source.label(),
                "layers": layers,
                "file": file,
                "remote": remote.map(|r| json!({"version": r.version, "serial": r.serial, "sha256": r.sha256, "fetched_at_ms": r.fetched_at_ms, "verified": state.as_ref().map(|s| s.verified.clone())})),
                "pinned": pins.get(&id).map(|p| p.version.clone()),
                "warnings": l.warnings,
            })
        })
        .collect();
    if is_json(g) {
        println!(
            "{}",
            json!({"manifests": rows, "channel": channel::status_json(&root), "warnings": set.warnings})
        );
        return EXIT_OK;
    }
    for r in &rows {
        let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
        let layers: Vec<String> = serde_json::from_value(r["layers"].clone()).unwrap_or_default();
        let mut line = format!("{:<14} {:<8} {}", s("id"), s("source"), layers.join(" < "));
        if let Some(rm) = r["remote"].as_object() {
            line.push_str(&format!(
                " · remote v{} ({})",
                rm.get("version").and_then(Value::as_str).unwrap_or("?"),
                rm.get("verified").and_then(Value::as_str).unwrap_or("?")
            ));
        }
        if let Some(p) = r["pinned"].as_str() {
            line.push_str(&format!(" · PINNED at v{p}"));
        }
        println!("{line}");
    }
    match &state {
        Some(st) => println!(
            "\nchannel: serial {} · {} · {}",
            st.serial, st.verified, st.url
        ),
        None => println!("\nchannel: nothing fetched yet (`vibeke integration update`)"),
    }
    EXIT_OK
}

/// `pin <id> [<version>|current]` / `unpin <id>`.
fn pin(g: &Global, unpin: bool, args: &[String]) -> i32 {
    use vk_server::agents::channel;
    let Some(id) = args.get(1).filter(|a| !a.starts_with("--")) else {
        eprintln!("{USAGE}");
        return EXIT_USAGE;
    };
    let root = channel::root();
    if unpin {
        return match channel::unpin(&root, id) {
            Ok(had) => {
                if is_json(g) {
                    println!("{}", json!({"id": id, "unpinned": had}));
                } else if had {
                    println!("{id}: unpinned; the next update may replace it");
                } else {
                    println!("{id}: was not pinned");
                }
                EXIT_OK
            }
            Err(e) => {
                eprintln!("vibeke integration unpin: {e}");
                EXIT_API
            }
        };
    }
    let version = args
        .get(2)
        .filter(|a| !a.starts_with("--"))
        .map(String::as_str);
    match channel::pin(&root, id, version) {
        Ok(p) => {
            if is_json(g) {
                println!(
                    "{}",
                    json!({"id": id, "version": p.version, "pinned_at_ms": p.pinned_at_ms})
                );
            } else {
                println!("{id}: pinned at v{}; updates leave it as it is", p.version);
            }
            EXIT_OK
        }
        Err(e) => {
            eprintln!("vibeke integration pin: {e}");
            EXIT_API
        }
    }
}

fn update(g: &Global, args: &[String]) -> i32 {
    let url = args
        .windows(2)
        .find(|w| w[0] == "--url")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| vk_server::agents::channel::DEFAULT_URL.to_string());
    let root = vk_server::agents::channel::root();
    match vk_server::agents::channel::update(&url, &root) {
        Ok(r) => {
            for w in &r.warnings {
                eprintln!("{w}");
            }
            if is_json(g) {
                println!(
                    "{}",
                    json!({"serial": r.serial, "applied": r.applied, "skipped": r.skipped, "unsigned": r.unsigned})
                );
            } else {
                println!(
                    "manifest channel serial {}: applied {}",
                    r.serial,
                    if r.applied.is_empty() {
                        "nothing".to_string()
                    } else {
                        r.applied.join(", ")
                    }
                );
                for s in &r.skipped {
                    println!("  skipped {s}");
                }
                println!(
                    "(a running server picks it up on restart or `vibeke api call agent.manifests_reload`)"
                );
            }
            EXIT_OK
        }
        Err(e) => {
            eprintln!("vibeke integration update: {e}");
            EXIT_API
        }
    }
}

/// `--mcp`: the MCP server entry for harnesses with an MCP client config (Claude, Codex).
fn run_mcp(verb: &str, hs: Vec<Harness>, dirs: &Dirs, write: bool, dry: bool) -> i32 {
    let hs: Vec<Harness> = hs
        .into_iter()
        .filter(|h| matches!(h, Harness::Claude | Harness::Codex))
        .collect();
    let mut code = EXIT_OK;
    for h in hs {
        match verb {
            "list" | "status" => match vk_agents::mcp_status(h, dirs) {
                Ok(st) => {
                    let state = if st.installed {
                        "installed"
                    } else if st.foreign {
                        "foreign entry (left alone)"
                    } else {
                        "not installed"
                    };
                    println!("{:<7} mcp {state:<26} {}", h.id(), st.file.display());
                }
                Err(e) => {
                    eprintln!("{}: {e:#}", h.id());
                    code = EXIT_API;
                }
            },
            "install" | "uninstall" => {
                let plan = if verb == "install" {
                    vk_agents::plan_mcp_install(h, dirs, &stable_bin())
                } else {
                    vk_agents::plan_mcp_uninstall(h, dirs)
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
                        "{}: mcp already {}",
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
                    println!("{}", f.diff());
                }
                for n in &plan.notes {
                    println!("note: {n}");
                }
                if dry || !write {
                    println!("{}: dry run — rerun with --yes to write", h.id());
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
            _ => {
                eprintln!("vibeke integration install|uninstall|status <claude|codex|all> --mcp");
                return EXIT_USAGE;
            }
        }
    }
    code
}

fn hs_contains_codex(target: Option<&str>) -> bool {
    matches!(target, None | Some("all") | Some("codex"))
}

/// Tamper detection baseline (09 §5.3): remember what install wrote, forget it on uninstall.
fn record_integrity(verb: &str, h: Harness, dirs: &Dirs) {
    let r = if verb == "install" {
        vk_server::integrity::record_install(h, dirs)
    } else {
        vk_server::integrity::forget(h)
    };
    if let Err(e) = r {
        eprintln!(
            "{}: could not record the integration fingerprint: {e}",
            h.id()
        );
    }
}
