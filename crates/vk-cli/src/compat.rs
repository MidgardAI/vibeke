//! `vibeke plugin …`, `vibeke compat …` and the `herdr`-compatible shim (07 §7.7, §8.2).
//!
//! * Registry operations (`install link unlink uninstall enable disable trust untrust list
//!   config-dir`) edit the per-user `plugins.json` directly, so they work with no server running.
//!   Nothing a plugin ships runs before `vibeke plugin trust <id> --legacy` (or `install --yes`).
//! * `plugin action list|run` and `plugin logs` go through the server API.
//! * `vibeke compat herdr <args>` (and the binary invoked as `herdr`) speaks the Herdr CLI
//!   grammar. It routes to the invocation's private broker when it runs inside a plugin
//!   invocation (`VIBEKE_HERDR_BROKER`), otherwise to this session's Vibeke server. It never
//!   uses a `HERDR_SOCKET_PATH` that is not a Vibeke broker, so it cannot reach a live Herdr.

use crate::client::{self, Client};
use crate::{EXIT_API, EXIT_NO_SERVER, EXIT_OK, EXIT_PERMISSION, EXIT_USAGE, Global};
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use vk_compat::herdr::cli::{Local, Parsed};
use vk_compat::herdr::registry::{self, Registry, RegistryError};
use vk_compat::herdr::{self, launch};

const PLUGIN_HELP: &str = "vibeke plugin — Herdr-compatible plugins (M5, partial)

  vibeke plugin list
  vibeke plugin install <dir | herdr-plugin.toml> [--yes] [--dry-run]
  vibeke plugin link <dir>
  vibeke plugin trust <id> --legacy      review and grant Herdr legacy trust (shows actions/events)
  vibeke plugin untrust <id>
  vibeke plugin enable|disable <id>
  vibeke plugin unlink <id> | uninstall <id>
  vibeke plugin config-dir <id>
  vibeke plugin action list [--plugin id]
  vibeke plugin action run <plugin> <action> | <plugin>.<action> [--pane P]
  vibeke plugin logs [--plugin id] [--limit n]
  vibeke plugin migrate --from <dir> [--plugin id]... [--dry-run] [--link]
                                         copy Herdr plugin config/state (source read only)
  vibeke plugin migrate --rollback       remove what the last migration created

Herdr plugins declare no capabilities; nothing they ship runs until trusted.";

fn as_json(g: &Global) -> bool {
    g.json.unwrap_or(!std::io::stdout().is_terminal())
}

fn fail(kind: &str, msg: impl std::fmt::Display, code: i32) -> i32 {
    eprintln!(
        "{}",
        json!({"error": {"kind": kind, "message": msg.to_string()}})
    );
    code
}

fn reg_fail(e: RegistryError) -> i32 {
    match e {
        RegistryError::NotFound(id) => {
            fail("not_found", format!("plugin not found: {id}"), EXIT_API)
        }
        RegistryError::Conflict(m) => fail("conflict", m, EXIT_API),
        RegistryError::Manifest(m) => fail("invalid_params", m, EXIT_API),
        other => fail("internal", other, EXIT_API),
    }
}

/// Inside a pane (agent scope): registry changes and trust are operator decisions (09 §6).
fn pane_scoped() -> bool {
    std::env::var_os("VIBEKE_PANE_TOKEN").is_some_and(|v| !v.is_empty())
}

/// Inside a plugin invocation (a broker is set): a plugin may not grant trust.
fn in_plugin() -> bool {
    std::env::var_os("VIBEKE_HERDR_BROKER").is_some()
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn positionals(args: &[String], valued: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if valued.contains(&a.as_str()) {
            i += 2;
            continue;
        }
        if !a.starts_with("--") {
            out.push(a.clone());
        }
        i += 1;
    }
    out
}

fn print(g: &Global, v: &Value, human: impl FnOnce() -> String) {
    if g.quiet {
        return;
    }
    if as_json(g) {
        println!("{v}");
    } else {
        println!("{}", human());
    }
}

/// Run `[[build]]` for a managed checkout with the build environment (no broker/context).
fn build(entry: &registry::Entry, m: &herdr::manifest::Manifest) -> Result<(), String> {
    for step in m.build_on(herdr::current_platform()) {
        let argv = launch::resolve_argv(&entry.root, &step.command);
        let env = launch::build_env(&entry.id, &entry.root, std::env::vars());
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&entry.root)
            .env_clear()
            .envs(env)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("build {}: {e}", argv.join(" ")))?;
        if !out.status.success() {
            return Err(format!(
                "build `{}` failed ({}): {}",
                argv.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    Ok(())
}

/// Grant legacy trust for `id`, then build a managed checkout that was not built yet. A build
/// failure revokes the grant again.
fn grant(dirs: &registry::PluginDirs, reg: &mut Registry, id: &str) -> Result<Value, i32> {
    let g = reg.trust(id).map_err(reg_fail)?;
    let entry = reg.get(id).map_err(reg_fail)?.clone();
    if entry.managed && !entry.built {
        let (m, _) = registry::read_manifest(&entry.root).map_err(reg_fail)?;
        if let Err(e) = build(&entry, &m) {
            reg.revoke(id).map_err(reg_fail)?;
            reg.save(dirs).map_err(reg_fail)?;
            return Err(fail("build_failed", e, EXIT_API));
        }
        reg.mark_built(id);
    }
    reg.save(dirs).map_err(reg_fail)?;
    Ok(json!(g))
}

fn entry_json(dirs: &registry::PluginDirs, e: &registry::Entry) -> Value {
    let (st, m) = registry::entry_status(e);
    json!({
        "plugin_id": e.id,
        "name": m.as_ref().and_then(|m| m.name.clone()),
        "version": m.as_ref().and_then(|m| m.version.clone()),
        "status": st.as_str(),
        "enabled": e.enabled,
        "managed": e.managed,
        "built": e.built,
        "root": e.root,
        "source": e.origin.path,
        "trust": e.trust,
        "config_dir": dirs.config_dir(&e.id),
        "state_dir": dirs.state_dir(&e.id),
        "warnings": m.as_ref().map(|m| m.warnings.clone()).unwrap_or_default(),
    })
}

/// Registry operations shared by `vibeke plugin` and the shim's `herdr plugin`.
fn local(g: &Global, op: Local) -> i32 {
    let dirs = vk_server::compat::plugin_dirs();
    let mutating = !matches!(op, Local::PluginList | Local::PluginConfigDir { .. });
    if mutating && pane_scoped() {
        return fail(
            "permission_denied",
            "plugin registry changes are not allowed from a pane; ask the user",
            EXIT_PERMISSION,
        );
    }
    let mut reg = match Registry::load(&dirs) {
        Ok(r) => r,
        Err(e) => return reg_fail(e),
    };
    match op {
        Local::PluginList => {
            let list: Vec<Value> = reg.plugins.values().map(|e| entry_json(&dirs, e)).collect();
            print(g, &json!({"plugins": list}), || {
                if list.is_empty() {
                    return "no plugins".into();
                }
                list.iter()
                    .map(|p| {
                        format!(
                            "{:<36} {:<12} {}",
                            p["plugin_id"].as_str().unwrap_or(""),
                            p["status"].as_str().unwrap_or(""),
                            p["root"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            EXIT_OK
        }
        Local::PluginConfigDir { id } => match reg.get(&id) {
            Ok(_) => {
                let d = dirs.config_dir(&id);
                let _ = std::fs::create_dir_all(&d);
                print(
                    g,
                    &json!({"plugin_id": id, "config_dir": d, "state_dir": dirs.state_dir(&id)}),
                    || d.display().to_string(),
                );
                EXIT_OK
            }
            Err(e) => reg_fail(e),
        },
        Local::PluginInstall {
            source,
            git_ref,
            yes,
        } => {
            let path = PathBuf::from(&source);
            if !path.exists() {
                let looks_remote = source.split('/').count() >= 2
                    && !source.starts_with('.')
                    && !source.starts_with('/');
                return fail(
                    "unsupported",
                    if looks_remote {
                        format!(
                            "{source}: installing from a git repository is not supported yet; clone it and install the directory"
                        )
                    } else {
                        format!("{source}: no such file or directory")
                    },
                    EXIT_API,
                );
            }
            if yes && in_plugin() {
                return fail(
                    "permission_denied",
                    "a plugin cannot accept legacy trust",
                    EXIT_PERMISSION,
                );
            }
            let (entry, m) = match reg.install(&dirs, &path, git_ref.as_deref()) {
                Ok(x) => x,
                Err(e) => return reg_fail(e),
            };
            if let Err(e) = reg.save(&dirs) {
                return reg_fail(e);
            }
            let digest = registry::read_manifest(&entry.root)
                .map(|(_, d)| d)
                .unwrap_or_default();
            let terms = registry::trust_terms(&entry, &m, &digest);
            let grant_v = if yes {
                match grant(&dirs, &mut reg, &entry.id) {
                    Ok(g) => Some(g),
                    Err(code) => {
                        // Abort registration on build failure (07 §7.7).
                        let _ = reg.uninstall(&dirs, &entry.id);
                        let _ = reg.save(&dirs);
                        return code;
                    }
                }
            } else {
                None
            };
            let e = reg.get(&entry.id).cloned().unwrap_or(entry);
            print(
                g,
                &json!({"plugin": entry_json(&dirs, &e), "grant": grant_v, "trust_terms": terms}),
                || {
                    if yes {
                        format!("{terms}\ninstalled and trusted: {}", e.id)
                    } else {
                        format!(
                            "{terms}\ninstalled {} (inactive). Grant trust with: vibeke plugin trust {} --legacy",
                            e.id, e.id
                        )
                    }
                },
            );
            EXIT_OK
        }
        Local::PluginLink { path, yes } => {
            if yes && in_plugin() {
                return fail(
                    "permission_denied",
                    "a plugin cannot accept legacy trust",
                    EXIT_PERMISSION,
                );
            }
            let (entry, m) = match reg.link(Path::new(&path)) {
                Ok(x) => x,
                Err(e) => return reg_fail(e),
            };
            if yes && let Err(e) = reg.trust(&entry.id) {
                return reg_fail(e);
            }
            if let Err(e) = reg.save(&dirs) {
                return reg_fail(e);
            }
            let digest = registry::read_manifest(&entry.root)
                .map(|(_, d)| d)
                .unwrap_or_default();
            let terms = registry::trust_terms(&entry, &m, &digest);
            let e = reg.get(&entry.id).cloned().unwrap_or(entry);
            print(
                g,
                &json!({"plugin": entry_json(&dirs, &e), "trust_terms": terms}),
                || {
                    format!(
                        "{terms}\nlinked {} ({})",
                        e.id,
                        registry::entry_status(&e).0.as_str()
                    )
                },
            );
            EXIT_OK
        }
        Local::PluginUnlink { id } => {
            match reg.unlink(&id).and_then(|e| reg.save(&dirs).map(|_| e)) {
                Ok(e) => {
                    print(g, &json!({"unlinked": e.id, "root": e.root}), || {
                        format!("unlinked {id} (files kept)")
                    });
                    EXIT_OK
                }
                Err(e) => reg_fail(e),
            }
        }
        Local::PluginUninstall { id } => {
            match reg
                .uninstall(&dirs, &id)
                .and_then(|e| reg.save(&dirs).map(|_| e))
            {
                Ok(e) => {
                    print(g, &json!({"uninstalled": e.id}), || {
                        format!("uninstalled {id} (config/state kept)")
                    });
                    EXIT_OK
                }
                Err(e) => reg_fail(e),
            }
        }
        Local::PluginEnable { id } => toggle(g, &dirs, &mut reg, &id, true),
        Local::PluginDisable { id } => toggle(g, &dirs, &mut reg, &id, false),
    }
}

fn toggle(g: &Global, dirs: &registry::PluginDirs, reg: &mut Registry, id: &str, on: bool) -> i32 {
    match reg
        .set_enabled(id, on)
        .and_then(|e| reg.save(dirs).map(|_| e))
    {
        Ok(e) => {
            let v = entry_json(dirs, &e);
            print(g, &json!({"plugin": v}), || {
                format!("{id}: {}", v["status"].as_str().unwrap_or(""))
            });
            EXIT_OK
        }
        Err(e) => reg_fail(e),
    }
}

fn trust_cmd(g: &Global, args: &[String]) -> i32 {
    let pos = positionals(args, &[]);
    let Some(id) = pos.first() else {
        eprintln!("vibeke plugin trust <id> --legacy");
        return EXIT_USAGE;
    };
    if pane_scoped() || in_plugin() {
        return fail(
            "permission_denied",
            "legacy trust is an operator decision; it cannot be granted from a pane or plugin",
            EXIT_PERMISSION,
        );
    }
    let dirs = vk_server::compat::plugin_dirs();
    let mut reg = match Registry::load(&dirs) {
        Ok(r) => r,
        Err(e) => return reg_fail(e),
    };
    let entry = match reg.get(id) {
        Ok(e) => e.clone(),
        Err(e) => return reg_fail(e),
    };
    let (m, digest) = match registry::read_manifest(&entry.root) {
        Ok(x) => x,
        Err(e) => return reg_fail(e),
    };
    let terms = registry::trust_terms(&entry, &m, &digest);
    if !flag(args, "--legacy") {
        if as_json(g) {
            eprintln!(
                "{}",
                json!({"error": {"kind": "permission_denied", "message": "add --legacy to grant Herdr legacy trust", "details": {"trust_terms": terms}}})
            );
        } else {
            eprintln!("{terms}\nRe-run with --legacy to grant this trust.");
        }
        return EXIT_PERMISSION;
    }
    match grant(&dirs, &mut reg, id) {
        Ok(gr) => {
            let e = reg.get(id).cloned().unwrap_or(entry);
            print(
                g,
                &json!({"plugin": entry_json(&dirs, &e), "grant": gr, "trust_terms": terms}),
                || {
                    format!(
                        "{terms}\ntrusted {id} ({})",
                        registry::entry_status(&e).0.as_str()
                    )
                },
            );
            EXIT_OK
        }
        Err(code) => code,
    }
}

fn untrust_cmd(g: &Global, args: &[String]) -> i32 {
    let pos = positionals(args, &[]);
    let Some(id) = pos.first() else {
        eprintln!("vibeke plugin untrust <id>");
        return EXIT_USAGE;
    };
    if pane_scoped() {
        return fail(
            "permission_denied",
            "not allowed from a pane",
            EXIT_PERMISSION,
        );
    }
    let dirs = vk_server::compat::plugin_dirs();
    let mut reg = match Registry::load(&dirs) {
        Ok(r) => r,
        Err(e) => return reg_fail(e),
    };
    match reg.revoke(id).and_then(|e| reg.save(&dirs).map(|_| e)) {
        Ok(e) => {
            print(g, &json!({"plugin": entry_json(&dirs, &e)}), || {
                format!("revoked trust for {id}")
            });
            EXIT_OK
        }
        Err(e) => reg_fail(e),
    }
}

/// Where migration records live (`$STATE/plugins/migrations`).
fn migration_records() -> PathBuf {
    vk_server::paths::state_root().join("plugins/migrations")
}

/// `vibeke plugin migrate --from <dir> [--plugin id]… [--dry-run] [--link]` and
/// `vibeke plugin migrate --rollback`: copy Herdr plugin config/state into Vibeke's plugin
/// dirs (07 §7.7). The source is only ever the directory the user names; it is read, never
/// changed. Conflicts are reported and left alone. `--link` also registers the source
/// registry's plugin directories (untrusted, nothing runs).
fn migrate_cmd(g: &Global, args: &[String]) -> i32 {
    if pane_scoped() || in_plugin() {
        return fail(
            "permission_denied",
            "plugin migration is an operator action; run it outside panes and plugins",
            EXIT_PERMISSION,
        );
    }
    let records = migration_records();
    if flag(args, "--rollback") {
        let Some((path, rec)) = herdr::migrate::latest(&records) else {
            return fail("not_found", "no migration to roll back", EXIT_API);
        };
        let rb = herdr::migrate::rollback(&rec);
        let done = path.with_extension("rolled-back");
        let _ = std::fs::rename(&path, &done);
        print(
            g,
            &json!({"rolled_back": rec.id, "from": rec.from, "result": rb}),
            || {
                format!(
                    "rolled back {}: removed {} path(s), kept {} changed file(s); {} was never modified",
                    rec.id,
                    rb.removed.len(),
                    rb.kept_changed.len(),
                    rec.from.display()
                )
            },
        );
        return EXIT_OK;
    }
    let Some(from) = value(args, "--from") else {
        eprintln!(
            "vibeke plugin migrate --from <herdr config dir or a copy> [--plugin id]... [--dry-run] [--link]\n\
             vibeke plugin migrate --rollback\n\
             The source is never assumed: name it explicitly. It is only read."
        );
        return EXIT_USAGE;
    };
    let only: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(i, a)| *a == "--plugin" && *i + 1 < args.len())
        .map(|(i, _)| args[i + 1].clone())
        .collect();
    let dirs = vk_server::compat::plugin_dirs();
    let plan = match herdr::migrate::plan(Path::new(&from), &dirs, &only) {
        Ok(p) => p,
        Err(e) => return fail("invalid_params", e, EXIT_API),
    };
    use herdr::migrate::Action;
    let summary = json!({
        "copy": plan.count(Action::Copy),
        "same": plan.count(Action::Same),
        "conflict": plan.count(Action::Conflict),
        "skipped": plan.count(Action::Skipped),
    });
    if flag(args, "--dry-run") {
        print(
            g,
            &json!({"dry_run": true, "plan": plan, "summary": summary}),
            || {
                let mut s = format!(
                    "from {} (read only): {} plugin(s), {summary}\n",
                    plan.from.display(),
                    plan.plugins.len()
                );
                for f in &plan.files {
                    s.push_str(&format!(
                        "  {:?} {} {} -> {}\n",
                        f.action,
                        f.plugin,
                        f.from.display(),
                        f.to.display()
                    ));
                }
                s
            },
        );
        return EXIT_OK;
    }
    let rec = match herdr::migrate::apply(&plan, &records) {
        Ok(r) => r,
        Err(e) => return fail("internal", e, EXIT_API),
    };
    let mut linked = Vec::new();
    let mut link_errors = Vec::new();
    if flag(args, "--link") {
        match Registry::load(&dirs) {
            Ok(mut reg) => {
                for p in plan.plugins.iter().filter(|p| p.has_manifest) {
                    let Some(root) = &p.root else { continue };
                    match reg.link(root) {
                        Ok((e, _)) => linked.push(e.id),
                        Err(e) => link_errors.push(format!("{}: {e}", p.id)),
                    }
                }
                if let Err(e) = reg.save(&dirs) {
                    return reg_fail(e);
                }
            }
            Err(e) => return reg_fail(e),
        }
    }
    let suggestions: Vec<String> = plan
        .plugins
        .iter()
        .filter(|p| p.has_manifest && !linked.contains(&p.id))
        .filter_map(|p| p.root.as_ref())
        .map(|r| format!("vibeke plugin link {}", r.display()))
        .collect();
    print(
        g,
        &json!({
            "migration": rec.id,
            "from": plan.from,
            "summary": summary,
            "created": rec.created.len(),
            "conflicts": rec.conflicts,
            "plugins": plan.plugins,
            "layouts": plan.layouts,
            "linked": linked,
            "link_errors": link_errors,
            "suggested": suggestions,
            "warnings": plan.warnings,
            "rollback": "vibeke plugin migrate --rollback",
        }),
        || {
            let mut s = format!(
                "migrated {} file(s) from {} (unchanged); {} conflict(s) left alone\n",
                plan.count(Action::Copy),
                plan.from.display(),
                rec.conflicts.len()
            );
            for c in &rec.conflicts {
                s.push_str(&format!("  conflict: {}\n", c.display()));
            }
            for l in &linked {
                s.push_str(&format!(
                    "  linked {l} (untrusted: review with `vibeke plugin trust {l} --legacy`)\n"
                ));
            }
            for x in &suggestions {
                s.push_str(&format!("  next: {x}\n"));
            }
            s.push_str("undo with `vibeke plugin migrate --rollback`");
            s
        },
    );
    EXIT_OK
}

async fn api_call(g: &Global, method: &str, params: Value) -> i32 {
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    let stream = match client::connect_or_spawn(&g.session, &socket, g.no_spawn).await {
        Ok(s) => s,
        Err(e) => return fail("server_unavailable", format!("{e:#}"), EXIT_NO_SERVER),
    };
    let mut c = Client::new(stream);
    crate::run_api(&mut c, g, method, params).await
}

/// `vibeke plugin …`.
pub async fn plugin_cmd(g: &Global, args: &[String]) -> i32 {
    let verb = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);
    let one = |what: &str| -> Result<String, i32> {
        positionals(rest, &[]).first().cloned().ok_or_else(|| {
            eprintln!("vibeke plugin {what} <id>");
            EXIT_USAGE
        })
    };
    match verb {
        "" | "help" | "--help" | "-h" => {
            println!("{PLUGIN_HELP}");
            if verb.is_empty() { EXIT_USAGE } else { EXIT_OK }
        }
        "list" | "ls" => local(g, Local::PluginList),
        "migrate" => migrate_cmd(g, rest),
        "install" => {
            let pos = positionals(rest, &["--ref"]);
            let [src] = pos.as_slice() else {
                eprintln!("vibeke plugin install <dir | herdr-plugin.toml> [--yes] [--dry-run]");
                return EXIT_USAGE;
            };
            if flag(rest, "--dry-run") {
                return match herdr::manifest::Manifest::load(Path::new(src)) {
                    Ok((m, text)) => {
                        let entry = registry::Entry {
                            id: m.id.clone(),
                            kind: "herdr".into(),
                            root: PathBuf::from(src),
                            managed: true,
                            origin: registry::Origin {
                                kind: "local".into(),
                                path: PathBuf::from(src),
                                requested_ref: None,
                            },
                            enabled: true,
                            built: false,
                            installed_at_ms: 0,
                            trust: None,
                        };
                        let terms = registry::trust_terms(
                            &entry,
                            &m,
                            &registry::sha256_hex(text.as_bytes()),
                        );
                        print(
                            g,
                            &json!({"dry_run": true, "manifest": m, "trust_terms": terms}),
                            || terms.clone(),
                        );
                        EXIT_OK
                    }
                    Err(e) => fail("invalid_params", e, EXIT_API),
                };
            }
            local(
                g,
                Local::PluginInstall {
                    source: src.clone(),
                    git_ref: value(rest, "--ref"),
                    yes: flag(rest, "--yes") || flag(rest, "-y"),
                },
            )
        }
        "link" => match one("link") {
            Ok(path) => local(
                g,
                Local::PluginLink {
                    path,
                    yes: flag(rest, "--yes"),
                },
            ),
            Err(c) => c,
        },
        "unlink" => match one("unlink") {
            Ok(id) => local(g, Local::PluginUnlink { id }),
            Err(c) => c,
        },
        "uninstall" | "remove" => match one("uninstall") {
            Ok(id) => local(g, Local::PluginUninstall { id }),
            Err(c) => c,
        },
        "enable" => match one("enable") {
            Ok(id) => local(g, Local::PluginEnable { id }),
            Err(c) => c,
        },
        "disable" => match one("disable") {
            Ok(id) => local(g, Local::PluginDisable { id }),
            Err(c) => c,
        },
        "config-dir" => match one("config-dir") {
            Ok(id) => local(g, Local::PluginConfigDir { id }),
            Err(c) => c,
        },
        "trust" => trust_cmd(g, rest),
        "untrust" | "revoke" => untrust_cmd(g, rest),
        "action" | "actions" => {
            let sub = rest.first().map(String::as_str).unwrap_or("list");
            let more = rest.get(1..).unwrap_or(&[]);
            match sub {
                "list" | "ls" => {
                    let mut p = json!({});
                    if let Some(pl) = value(more, "--plugin") {
                        p["plugin"] = json!(pl);
                    }
                    api_call(g, "plugin.action.list", p).await
                }
                "run" | "invoke" => {
                    let pos = positionals(more, &["--pane", "--workspace", "--tab"]);
                    let mut p = match pos.as_slice() {
                        [q] => json!({"action": q}),
                        [pl, a] => json!({"plugin": pl, "action": a}),
                        _ => {
                            eprintln!(
                                "vibeke plugin action run <plugin> <action> | <plugin>.<action>"
                            );
                            return EXIT_USAGE;
                        }
                    };
                    for k in ["pane", "workspace", "tab"] {
                        if let Some(v) = value(more, &format!("--{k}")) {
                            p[k] = json!(v);
                        }
                    }
                    api_call(g, "plugin.action.run", p).await
                }
                _ => {
                    eprintln!("vibeke plugin action list | run <plugin> <action>");
                    EXIT_USAGE
                }
            }
        }
        "log" | "logs" => {
            let mut p = json!({});
            if let Some(pl) = value(rest, "--plugin")
                .or_else(|| positionals(rest, &["--plugin", "--limit"]).first().cloned())
            {
                p["plugin"] = json!(pl);
            }
            if let Some(n) = value(rest, "--limit").and_then(|n| n.parse::<u64>().ok()) {
                p["limit"] = json!(n);
            }
            api_call(g, "plugin.log.list", p).await
        }
        other => {
            eprintln!("vibeke plugin: unknown verb `{other}`\n\n{PLUGIN_HELP}");
            EXIT_USAGE
        }
    }
}

// ---- the herdr shim ---------------------------------------------------------------------------

/// The invocation's broker, only when it is a Vibeke broker under this machine's runtime root.
fn broker() -> Option<PathBuf> {
    let b = std::env::var_os("VIBEKE_HERDR_BROKER").map(PathBuf::from)?;
    let herdr_sock = std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from);
    (herdr_sock.as_deref() == Some(b.as_path())
        && b.starts_with(vk_server::paths::runtime_root())
        && b.exists())
    .then_some(b)
}

/// A session's public compat listener (Herdr layout under `$RUNTIME/herdr-compat`), if running.
fn listener(session: &str) -> Option<PathBuf> {
    let root = vk_server::paths::runtime_root().join("herdr-compat");
    let p = herdr::session_socket(&root, session);
    p.exists().then_some(p)
}

/// The Vibeke session a broker belongs to (`$RUNTIME/<session>/herdr-compat/brokers/…`).
fn broker_session(b: &Path) -> Option<String> {
    let rel = b.strip_prefix(vk_server::paths::runtime_root()).ok()?;
    rel.components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
}

/// A request to an explicitly selected session through its native socket (never spawns a
/// server). From a plugin invocation the destination re-checks the plugin's grant (09 §6).
async fn session_call(
    session: &str,
    method: &str,
    params: Value,
    as_plugin: Option<String>,
) -> i32 {
    let socket = vk_server::paths::Paths::new(session).socket();
    let stream = match client::connect(&socket).await {
        Ok(s) => s,
        Err(e) => {
            return fail(
                "server_unavailable",
                format!("session {session} is not running: {e:#}"),
                EXIT_NO_SERVER,
            );
        }
    };
    let mut c = Client::new(stream);
    if let Err(e) = c.hello("herdr-shim").await {
        crate::print_error(&e);
        return crate::exit_code_for(&e);
    }
    let mut p = json!({"method": method, "params": params});
    if let Some(id) = as_plugin {
        p["as_plugin"] = json!(id);
    }
    match c.call("compat.herdr.call", p).await {
        Ok(v) => print_herdr(&v.to_string()),
        Err(e) => {
            crate::print_error(&e);
            crate::exit_code_for(&e)
        }
    }
}

/// One raw Herdr request; prints the result (or streams events). Returns the exit code.
async fn raw(sock: &Path, method: &str, params: Value) -> i32 {
    let stream = match tokio::net::UnixStream::connect(sock).await {
        Ok(s) => s,
        Err(e) => {
            return fail(
                "server_unavailable",
                format!("{}: {e}", sock.display()),
                EXIT_NO_SERVER,
            );
        }
    };
    let (rd, mut wr) = stream.into_split();
    let line = json!({"id": "1", "method": method, "params": params}).to_string() + "\n";
    if wr.write_all(line.as_bytes()).await.is_err() {
        return fail("server_unavailable", "write failed", EXIT_NO_SERVER);
    }
    let mut rd = BufReader::new(rd).lines();
    let first = match rd.next_line().await {
        Ok(Some(l)) => l,
        _ => return fail("server_unavailable", "no response", EXIT_API),
    };
    let code = print_herdr(&first);
    if code != EXIT_OK || method != "events.subscribe" {
        return code;
    }
    while let Ok(Some(l)) = rd.next_line().await {
        println!("{l}");
    }
    EXIT_OK
}

/// Print a Herdr response line: `result` on stdout, `error` on stderr (exit 1).
fn print_herdr(line: &str) -> i32 {
    let v: Value = serde_json::from_str(line).unwrap_or(Value::Null);
    if let Some(r) = v.get("result") {
        println!("{r}");
        EXIT_OK
    } else if let Some(e) = v.get("error") {
        eprintln!("{}", json!({"error": e}));
        if e["code"] == "permission_denied" {
            EXIT_PERMISSION
        } else {
            EXIT_API
        }
    } else {
        fail("internal", format!("unexpected response: {line}"), EXIT_API)
    }
}

/// `vibeke compat herdr <args>` / `herdr <args>`.
///
/// Session selection (07 §8.2): Herdr's global `--session NAME`, then `HERDR_SESSION`, then
/// the invocation's own session (its broker's, else `VIBEKE_SESSION`/`--session` of vibeke).
/// Selecting another session from a pane is refused; from a plugin invocation the destination
/// re-checks the plugin's grant. An explicitly selected session is never spawned.
pub async fn herdr_shim(g: &Global, args: &[String]) -> i32 {
    let (selected, args) = match herdr::cli::take_session(args) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };
    let selected = selected.or_else(|| {
        std::env::var("HERDR_SESSION")
            .ok()
            .filter(|s| herdr::valid_session_name(s))
    });
    match herdr::cli::parse(&args) {
        Parsed::Version => {
            println!(
                "herdr {} (Vibeke {} compatibility layer, partial)",
                herdr::BASELINE_VERSION,
                vk_proto::VERSION
            );
            EXIT_OK
        }
        Parsed::Help(h) => {
            println!("{h}");
            EXIT_OK
        }
        Parsed::Usage(u) => {
            eprintln!("{u}");
            EXIT_USAGE
        }
        Parsed::Refused { command, reason } => fail(
            "unsupported",
            format!("herdr {command}: {reason}"),
            EXIT_API,
        ),
        Parsed::Local(op) => {
            // The Herdr CLI prints JSON; so does the shim. The registry is per user, shared by
            // every session, so session selection does not change it.
            let g = Global {
                json: Some(true),
                ..g.clone()
            };
            local(&g, op)
        }
        Parsed::Call { method, params } => {
            let broker = broker();
            // Inside a plugin invocation whose broker is gone (the action ended, the grant was
            // revoked): never fall back to the user-level socket, which would bypass the grant.
            if in_plugin() && broker.is_none() {
                return fail(
                    "permission_denied",
                    "this plugin invocation's broker is closed (the invocation ended or its grant was revoked)",
                    EXIT_PERMISSION,
                );
            }
            let current = broker
                .as_deref()
                .and_then(broker_session)
                .unwrap_or_else(|| g.session.clone());
            let target = selected.unwrap_or_else(|| current.clone());
            if target != current {
                if pane_scoped() {
                    return fail(
                        "permission_denied",
                        "a pane cannot select another session",
                        EXIT_PERMISSION,
                    );
                }
                if method == "events.subscribe" {
                    return match listener(&target) {
                        Some(l) if broker.is_none() => raw(&l, &method, params).await,
                        _ => fail(
                            "unsupported",
                            format!(
                                "events.subscribe to session {target} needs its compat listener (and is not available to plugins across sessions)"
                            ),
                            EXIT_API,
                        ),
                    };
                }
                let as_plugin = broker
                    .as_ref()
                    .and_then(|_| std::env::var("HERDR_PLUGIN_ID").ok());
                return session_call(&target, &method, params, as_plugin).await;
            }
            if let Some(b) = broker {
                return raw(&b, &method, params).await;
            }
            if method == "events.subscribe" {
                return match listener(&target) {
                    Some(l) => raw(&l, &method, params).await,
                    None => fail(
                        "unsupported",
                        "events.subscribe needs the compat listener ([compat.herdr] enabled = true) or a plugin broker",
                        EXIT_API,
                    ),
                };
            }
            let socket = client::socket_path(&g.session, g.socket.as_deref());
            let stream = match client::connect_or_spawn(&g.session, &socket, g.no_spawn).await {
                Ok(s) => s,
                Err(e) => return fail("server_unavailable", format!("{e:#}"), EXIT_NO_SERVER),
            };
            let mut c = Client::new(stream);
            if let Err(e) = c.hello("herdr-shim").await {
                crate::print_error(&e);
                return crate::exit_code_for(&e);
            }
            match c
                .call(
                    "compat.herdr.call",
                    json!({"method": method, "params": params}),
                )
                .await
            {
                Ok(v) => print_herdr(&v.to_string()),
                Err(e) => {
                    crate::print_error(&e);
                    crate::exit_code_for(&e)
                }
            }
        }
    }
}

/// Entry point when the binary runs as `herdr` (private launcher or installed shim).
pub async fn herdr_main(args: Vec<String>) -> i32 {
    let g = Global {
        session: std::env::var("VIBEKE_SESSION").unwrap_or_else(|_| "default".into()),
        json: Some(true),
        ..Default::default()
    };
    herdr_shim(&g, &args).await
}

/// The Vibeke-managed directory for the optional `herdr` shim.
pub fn shim_dir() -> PathBuf {
    vk_server::paths::data_root().join("compat/bin")
}

/// `vibeke compat herdr|install-shim|uninstall-shim|status`.
pub async fn compat_cmd(g: &Global, args: &[String]) -> i32 {
    let verb = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);
    match verb {
        "herdr" => herdr_shim(g, rest).await,
        "install-shim" => {
            let dir = value(rest, "--dir")
                .map(PathBuf::from)
                .unwrap_or_else(shim_dir);
            let managed = vk_server::paths::data_root();
            if !dir.starts_with(&managed) || vk_server::compat::is_herdr_owned(&dir) {
                return fail(
                    "permission_denied",
                    format!(
                        "the herdr shim is only installed under {} (Vibeke-managed)",
                        managed.display()
                    ),
                    EXIT_PERMISSION,
                );
            }
            let exe = match std::env::current_exe() {
                Ok(e) => e,
                Err(e) => return fail("internal", e, EXIT_API),
            };
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return fail("internal", e, EXIT_API);
            }
            let link = dir.join("herdr");
            let _ = std::fs::remove_file(&link);
            if let Err(e) = std::os::unix::fs::symlink(&exe, &link) {
                return fail("internal", e, EXIT_API);
            }
            print(g, &json!({"shim": link, "target": exe}), || {
                format!(
                    "installed {} → {}\nAdd {} to PATH (before any real Herdr) for external automation.",
                    link.display(),
                    exe.display(),
                    dir.display()
                )
            });
            EXIT_OK
        }
        "uninstall-shim" => {
            let link = shim_dir().join("herdr");
            let removed =
                std::fs::symlink_metadata(&link).is_ok() && std::fs::remove_file(&link).is_ok();
            print(g, &json!({"removed": removed, "shim": link}), || {
                if removed {
                    format!("removed {}", link.display())
                } else {
                    "no shim installed".into()
                }
            });
            EXIT_OK
        }
        "status" => api_call(g, "compat.status", json!({})).await,
        _ => {
            eprintln!(
                "vibeke compat herdr <herdr args…>   run a Herdr CLI command against Vibeke\n\
                 vibeke compat install-shim          link `herdr` into {} (on request only)\n\
                 vibeke compat uninstall-shim\n\
                 vibeke compat status                baseline, listener, inventory coverage",
                shim_dir().display()
            );
            if verb.is_empty() || verb == "--help" {
                EXIT_OK
            } else {
                EXIT_USAGE
            }
        }
    }
}
