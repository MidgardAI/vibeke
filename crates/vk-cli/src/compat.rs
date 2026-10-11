//! `vibeke plugin …`, `vibeke compat …` and the `herdr`-compatible shim (07 §7.7, §8.2).
//!
//! * Registry operations (`install link unlink uninstall enable disable trust untrust list
//!   config-dir`) edit the per-user `plugins.json` directly, so they work with no server running.
//!   Nothing a plugin ships runs before `vibeke plugin trust <id> --legacy` (or `install --yes`).
//! * `plugin action list|run` and `plugin logs` go through the server API.
//! * `vibeke compat herdr <args>` (and the binary invoked as `herdr`) speaks the Herdr CLI
//!   grammar. It routes to the invocation's private broker when it runs inside a plugin
//!   invocation (`VIBEKE_HERDR_BROKER`), otherwise to this session's Vibeke server. A broker path
//!   is used only after `vk_server::compat::registered_broker` has resolved it to a socket the
//!   session registered (no `..`, no symlinks), so it cannot reach a live Herdr; `server.stop`
//!   and other server lifecycle methods are never forwarded anywhere.

use crate::client::{self, Client};
use crate::{EXIT_API, EXIT_NO_SERVER, EXIT_OK, EXIT_PERMISSION, EXIT_USAGE, Global};
use serde_json::{Value, json};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use vk_compat::herdr::cli::{ApiSchema, Local, Output, Parsed, StatusScope};
use vk_compat::herdr::registry::{self, Registry, RegistryError};
use vk_compat::herdr::{self, launch, source};

const PLUGIN_HELP: &str = "vibeke plugin — Herdr-compatible plugins (M5, partial)

  vibeke plugin list
  vibeke plugin install <dir | herdr-plugin.toml | owner/repo[/subdir][@ref]> [--ref R] [--yes] [--dry-run]
  vibeke plugin update <id> [--yes]      re-fetch a repository install (a new commit needs review)
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

/// Run `[[build]]` for a managed checkout with the build environment (no broker/context). The
/// manifest must stay the reviewed one (`digest`) before every step and after the last one;
/// a change aborts the build (and with it the registration).
fn build(
    entry: &registry::Entry,
    m: &herdr::manifest::Manifest,
    digest: &str,
) -> Result<(), String> {
    let unchanged = || match registry::read_manifest(&entry.root) {
        Ok((_, d)) if d == digest => Ok(()),
        Ok(_) => Err(format!(
            "{}'s manifest changed during the build; build and registration aborted",
            entry.id
        )),
        Err(e) => Err(format!("manifest unreadable during the build: {e}")),
    };
    for step in m.build_on(herdr::current_platform()) {
        unchanged()?;
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
    unchanged()
}

/// Grant legacy trust for `id`, then build a managed checkout. Each registry step is its own
/// locked read-modify-write (the build runs without the lock), and the build is recorded only
/// for the grant it was made for: a revocation made meanwhile stays. A build failure or a
/// manifest change during the build revokes the grant again.
fn grant(dirs: &registry::PluginDirs, id: &str) -> Result<Value, i32> {
    let (g, entry) = Registry::update(dirs, |reg| {
        let g = reg.trust(id)?;
        Ok((g, reg.get(id)?.clone()))
    })
    .map_err(reg_fail)?;
    if entry.managed {
        let revoke_this = |why: String| {
            let _ = Registry::update(dirs, |reg| {
                if reg
                    .get(id)
                    .ok()
                    .and_then(|e| e.trust.as_ref())
                    .is_some_and(|t| t.grant_id == g.grant_id)
                {
                    reg.revoke(id)?;
                }
                Ok(())
            });
            fail("build_failed", why, EXIT_API)
        };
        let m = match registry::read_manifest(&entry.root) {
            Ok((m, d)) if d == g.manifest_sha256 => m,
            Ok(_) => return Err(revoke_this("the manifest changed after review".into())),
            Err(e) => return Err(revoke_this(e.to_string())),
        };
        // Managed checkouts are read-only on disk except while their build runs.
        if let Err(e) = registry::set_tree_writable(&entry.root, true) {
            return Err(revoke_this(format!("{}: {e}", entry.root.display())));
        }
        let built = build(&entry, &m, &g.manifest_sha256);
        let sealed = registry::set_tree_writable(&entry.root, false);
        if let Err(e) = built {
            return Err(revoke_this(e));
        }
        if let Err(e) = sealed {
            return Err(revoke_this(format!("{}: {e}", entry.root.display())));
        }
        if let Err(e) = Registry::update(dirs, |reg| reg.finish_build(id, &g.grant_id)) {
            return Err(revoke_this(e.to_string()));
        }
    }
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
        "origin": e.origin,
        "trust": e.trust,
        "config_dir": dirs.config_dir(&e.id),
        "state_dir": dirs.state_dir(&e.id),
        "warnings": m.as_ref().map(|m| m.warnings.clone()).unwrap_or_default(),
    })
}

/// Registry operations shared by `vibeke plugin` and the shim's `herdr plugin`.
fn local(g: &Global, op: Local) -> i32 {
    let dirs = vk_server::compat::plugin_dirs();
    let mutating = !matches!(op, Local::PluginList { .. } | Local::PluginConfigDir { .. });
    if mutating && pane_scoped() {
        return fail(
            "permission_denied",
            "plugin registry changes are not allowed from a pane; ask the user",
            EXIT_PERMISSION,
        );
    }
    // Reads use a snapshot; every change is a locked read-modify-write (`Registry::update`).
    let reg = match Registry::load(&dirs) {
        Ok(r) => r,
        Err(e) => return reg_fail(e),
    };
    let current = |id: &str| {
        Registry::load(&dirs)
            .ok()
            .and_then(|r| r.get(id).ok().cloned())
    };
    match op {
        Local::PluginUpdate { id, yes } => {
            // Update = reinstall from the recorded repository and requested ref. A new commit
            // or manifest leaves the plugin inactive until it is reviewed again.
            let e = match reg.get(&id) {
                Ok(e) => e.clone(),
                Err(e) => return reg_fail(e),
            };
            let Some(repo) = e.origin.repo.clone() else {
                return fail(
                    "invalid_params",
                    format!(
                        "{id} was not installed from a repository; there is nothing to re-fetch"
                    ),
                    EXIT_API,
                );
            };
            local(
                g,
                Local::PluginInstall {
                    source: repo,
                    git_ref: e.origin.requested_ref.clone(),
                    yes,
                },
            )
        }
        Local::PluginList { .. } => {
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
            let remote = if path.exists() {
                None
            } else {
                match source::parse(&source, git_ref.as_deref()) {
                    Ok(Some(s)) => Some(s),
                    Ok(None) => {
                        return fail(
                            "invalid_params",
                            format!("{source}: no such file or directory"),
                            EXIT_API,
                        );
                    }
                    Err(e) => return fail("invalid_params", e, EXIT_USAGE),
                }
            };
            if yes && in_plugin() {
                return fail(
                    "permission_denied",
                    "a plugin cannot accept legacy trust",
                    EXIT_PERMISSION,
                );
            }
            // The copy is staged and verified outside the registry lock; the lock is held only
            // to publish it (new immutable checkout path, origin and grant in one save).
            let (entry, m) = match remote {
                None => {
                    let staged = match registry::stage_local(&dirs, &path, git_ref.as_deref()) {
                        Ok(s) => s,
                        Err(e) => return reg_fail(e),
                    };
                    match Registry::update(&dirs, |r| r.install_staged(&dirs, staged)) {
                        Ok(x) => x,
                        Err(e) => return reg_fail(e),
                    }
                }
                Some(src) => {
                    let fetched = match source::fetch(&src, &source::base(), &dirs.fetch_dir()) {
                        Ok(f) => f,
                        Err(e) => return fail("fetch_failed", e, EXIT_API),
                    };
                    let staged = registry::stage_git(&dirs, &fetched, &src);
                    let _ = std::fs::remove_dir_all(&fetched.work);
                    let staged = match staged {
                        Ok(s) => s,
                        Err(e) => return reg_fail(e),
                    };
                    let r = Registry::update(&dirs, |r| r.install_staged(&dirs, staged));
                    match r {
                        Ok(x) => x,
                        Err(e) => return reg_fail(e),
                    }
                }
            };
            let digest = registry::read_manifest(&entry.root)
                .map(|(_, d)| d)
                .unwrap_or_default();
            let terms = registry::trust_terms(&entry, &m, &digest);
            let grant_v = if yes {
                match grant(&dirs, &entry.id) {
                    Ok(g) => Some(g),
                    Err(code) => {
                        // Abort registration on build failure (07 §7.7).
                        let _ = Registry::update(&dirs, |r| r.uninstall(&dirs, &entry.id));
                        return code;
                    }
                }
            } else {
                None
            };
            let e = current(&entry.id).unwrap_or(entry);
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
        Local::PluginLink { path, yes, enabled } => {
            if yes && in_plugin() {
                return fail(
                    "permission_denied",
                    "a plugin cannot accept legacy trust",
                    EXIT_PERMISSION,
                );
            }
            let (entry, m) = match Registry::update(&dirs, |r| {
                let x = r.link(Path::new(&path))?;
                if yes {
                    r.trust(&x.0.id)?;
                }
                if !enabled {
                    r.set_enabled(&x.0.id, false)?;
                }
                Ok(x)
            }) {
                Ok(x) => x,
                Err(e) => return reg_fail(e),
            };
            let digest = registry::read_manifest(&entry.root)
                .map(|(_, d)| d)
                .unwrap_or_default();
            let terms = registry::trust_terms(&entry, &m, &digest);
            let e = current(&entry.id).unwrap_or(entry);
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
        Local::PluginUnlink { id } => match Registry::update(&dirs, |r| r.unlink(&id)) {
            Ok(e) => {
                print(g, &json!({"unlinked": e.id, "root": e.root}), || {
                    format!("unlinked {id} (files kept)")
                });
                EXIT_OK
            }
            Err(e) => reg_fail(e),
        },
        Local::PluginUninstall { id } => {
            match Registry::update(&dirs, |r| r.uninstall(&dirs, &id)) {
                Ok(e) => {
                    print(g, &json!({"uninstalled": e.id}), || {
                        format!("uninstalled {id} (config/state kept)")
                    });
                    EXIT_OK
                }
                Err(e) => reg_fail(e),
            }
        }
        Local::PluginEnable { id } => toggle(g, &dirs, &id, true),
        Local::PluginDisable { id } => toggle(g, &dirs, &id, false),
    }
}

/// `install --dry-run`: show the manifest and the trust terms for a directory or a repository
/// source (fetched into a scratch directory that is removed again; nothing is registered).
fn install_dry_run(g: &Global, src: &str, git_ref: Option<&str>) -> i32 {
    let dirs = vk_server::compat::plugin_dirs();
    let mut fetched = None;
    let mut origin = registry::Origin {
        kind: "local".into(),
        path: PathBuf::from(src),
        requested_ref: git_ref.map(str::to_string),
        repo: None,
        commit: None,
    };
    let mut dir = PathBuf::from(src);
    if !dir.exists() {
        match source::parse(src, git_ref) {
            Ok(Some(s)) => match source::fetch(&s, &source::base(), &dirs.fetch_dir()) {
                Ok(f) => {
                    origin.kind = "git".into();
                    origin.path = PathBuf::from(&f.url);
                    origin.repo = Some(s.spec());
                    origin.commit = Some(f.commit.clone());
                    origin.requested_ref = s.git_ref.clone();
                    dir = f.plugin_dir.clone();
                    fetched = Some(f);
                }
                Err(e) => return fail("fetch_failed", e, EXIT_API),
            },
            Ok(None) => {
                return fail(
                    "invalid_params",
                    format!("{src}: no such file or directory"),
                    EXIT_API,
                );
            }
            Err(e) => return fail("invalid_params", e, EXIT_USAGE),
        }
    }
    let loaded = herdr::manifest::Manifest::load(&dir);
    if let Some(f) = &fetched {
        let _ = std::fs::remove_dir_all(&f.work);
    }
    match loaded {
        Ok((m, text)) => {
            let entry = registry::Entry {
                id: m.id.clone(),
                kind: "herdr".into(),
                root: dir,
                managed: true,
                origin,
                enabled: true,
                built: false,
                installed_at_ms: 0,
                trust: None,
            };
            let terms = registry::trust_terms(&entry, &m, &registry::sha256_hex(text.as_bytes()));
            print(
                g,
                &json!({"dry_run": true, "manifest": m, "trust_terms": terms, "commit": entry.origin.commit}),
                || terms.clone(),
            );
            EXIT_OK
        }
        Err(e) => fail("invalid_params", e, EXIT_API),
    }
}

fn toggle(g: &Global, dirs: &registry::PluginDirs, id: &str, on: bool) -> i32 {
    match Registry::update(dirs, |r| r.set_enabled(id, on)) {
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
    let reg = match Registry::load(&dirs) {
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
    match grant(&dirs, id) {
        Ok(gr) => {
            let e = Registry::load(&dirs)
                .ok()
                .and_then(|r| r.get(id).ok().cloned())
                .unwrap_or(entry);
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
    match Registry::update(&dirs, |r| r.revoke(id)) {
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
        herdr::migrate::retire(&path);
        print(
            g,
            &json!({"rolled_back": rec.id, "from": rec.from, "complete": rec.complete, "result": rb}),
            || {
                format!(
                    "rolled back {}{}: removed {} path(s), kept {} changed file(s), refused {} unsafe path(s); {} was never modified",
                    rec.id,
                    if rec.complete {
                        ""
                    } else {
                        " (a migration that had failed part-way)"
                    },
                    rb.removed.len(),
                    rb.kept_changed.len(),
                    rb.refused.len(),
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
        Err(e) => {
            return fail(
                "internal",
                format!(
                    "{e}; the files copied so far are recorded: retry the same command to continue, or undo with `vibeke plugin migrate --rollback`"
                ),
                EXIT_API,
            );
        }
    };
    let mut linked = Vec::new();
    let mut link_errors = Vec::new();
    if flag(args, "--link") {
        let r = Registry::update(&dirs, |reg| {
            for p in plan.plugins.iter().filter(|p| p.has_manifest) {
                let Some(root) = &p.root else { continue };
                match reg.link(root) {
                    Ok((e, _)) => linked.push(e.id),
                    Err(e) => link_errors.push(format!("{}: {e}", p.id)),
                }
            }
            Ok(())
        });
        if let Err(e) = r {
            return reg_fail(e);
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

/// Tell a running server that the registry changed (best effort, never starts a server): it
/// refreshes key bindings and agent views and emits `plugin.registry_changed`.
async fn notify_registry(g: &Global) {
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    let Ok(stream) = client::connect_or_spawn(&g.session, &socket, true).await else {
        return;
    };
    let mut c = Client::new(stream);
    if c.hello("cli").await.is_ok() {
        let _ = c.call("plugin.registry.notify", json!({})).await;
    }
}

/// `vibeke plugin …`.
pub async fn plugin_cmd(g: &Global, args: &[String]) -> i32 {
    let code = plugin_cmd_inner(g, args).await;
    let verb = args.first().map(String::as_str).unwrap_or("");
    if code == EXIT_OK
        && matches!(
            verb,
            "install"
                | "update"
                | "link"
                | "unlink"
                | "uninstall"
                | "remove"
                | "enable"
                | "disable"
                | "trust"
                | "untrust"
                | "revoke"
                | "migrate"
                | "consent"
                | "import"
        )
    {
        notify_registry(g).await;
        // 09 §11: plugin installs, updates and trust changes are audited (no server needed).
        vk_server::audit::record_offline(
            &g.session,
            &format!("plugin.{}", verb.replace('-', "_")),
            serde_json::json!({"plugin": args.iter().skip(1).find(|a| !a.starts_with('-'))}),
            serde_json::json!({"args": args.iter().skip(1).filter(|a| a.starts_with("--")).collect::<Vec<_>>()}),
        );
    }
    code
}

/// A fetched repository turned out to hold a Herdr manifest: install it the Herdr way.
pub fn install_herdr_fallback(
    g: &Global,
    src: &str,
    git_ref: Option<&str>,
    args: &[String],
) -> i32 {
    if flag(args, "--dry-run") {
        return install_dry_run(g, src, git_ref);
    }
    local(
        g,
        Local::PluginInstall {
            source: src.to_string(),
            git_ref: git_ref.map(str::to_string),
            yes: flag(args, "--yes") || flag(args, "-y"),
        },
    )
}

async fn plugin_cmd_inner(g: &Global, args: &[String]) -> i32 {
    // Native plugins (vibeke-plugin.toml) first; Herdr verbs fall through.
    if let Some(code) = crate::plugin_native::cmd(g, args).await {
        return code;
    }
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
            println!("{PLUGIN_HELP}\n\n{}", crate::plugin_native::HELP);
            if verb.is_empty() { EXIT_USAGE } else { EXIT_OK }
        }
        "list" | "ls" => local(
            g,
            Local::PluginList {
                plugin: None,
                json: false,
            },
        ),
        "migrate" => migrate_cmd(g, rest),
        "install" => {
            let pos = positionals(rest, &["--ref"]);
            let [src] = pos.as_slice() else {
                eprintln!(
                    "vibeke plugin install <dir | herdr-plugin.toml | owner/repo[/subdir][@ref]> [--ref R] [--yes] [--dry-run]"
                );
                return EXIT_USAGE;
            };
            if flag(rest, "--dry-run") {
                return install_dry_run(g, src, value(rest, "--ref").as_deref());
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
        "update" => match one("update") {
            Ok(id) => {
                // Update = reinstall from the recorded repository and requested ref. A new
                // commit or manifest leaves the plugin inactive until it is reviewed again.
                let dirs = vk_server::compat::plugin_dirs();
                let e = match Registry::load(&dirs).and_then(|r| r.get(&id).cloned()) {
                    Ok(e) => e,
                    Err(e) => return reg_fail(e),
                };
                let Some(repo) = e.origin.repo.clone() else {
                    return fail(
                        "invalid_params",
                        format!("{id} was not installed from a repository"),
                        EXIT_API,
                    );
                };
                local(
                    g,
                    Local::PluginInstall {
                        source: repo,
                        git_ref: e.origin.requested_ref.clone(),
                        yes: flag(rest, "--yes"),
                    },
                )
            }
            Err(c) => c,
        },
        "link" => match one("link") {
            Ok(path) => local(
                g,
                Local::PluginLink {
                    path,
                    yes: flag(rest, "--yes"),
                    enabled: true,
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

/// The invocation's broker `(canonical socket, its session)`, only when `VIBEKE_HERDR_BROKER`
/// equals `HERDR_SOCKET_PATH` and resolves to a broker the session registered.
fn broker() -> Option<(PathBuf, String)> {
    let b = std::env::var_os("VIBEKE_HERDR_BROKER").map(PathBuf::from)?;
    let herdr_sock = std::env::var_os("HERDR_SOCKET_PATH").map(PathBuf::from);
    if herdr_sock.as_deref() != Some(b.as_path()) {
        return None;
    }
    vk_server::compat::registered_broker(&b).ok()
}

/// One request line. To the invocation's own broker the shim adds the invocation's secret
/// (`VIBEKE_HERDR_TOKEN`) as `vibeke_token`, so the broker serves it even from a process it
/// cannot place in the invocation's process tree; it is never sent anywhere else.
fn request_line(sock: &Path, id: &str, method: &str, params: Value) -> String {
    let mut req = json!({"id": id, "method": method, "params": params});
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let to_broker = std::env::var_os("VIBEKE_HERDR_BROKER")
        .is_some_and(|b| Path::new(&b) == sock || canon(Path::new(&b)) == canon(sock));
    if to_broker && let Ok(t) = std::env::var("VIBEKE_HERDR_TOKEN") {
        req["vibeke_token"] = json!(t);
    }
    if to_broker {
        // Lets the broker's test-only trace tell launcher calls from raw socket clients
        // (spec 18 §9); never used for authorization.
        let (k, v) = vk_server::compat::trace::SHIM_MARK;
        req[k] = json!(v);
    }
    req.to_string() + "\n"
}

/// A test-only launcher trace line (spec 18 §9): written only inside a plugin
/// (`HERDR_PLUGIN_ID`) while `VIBEKE_HERDR_TRACE` tracing is on.
fn trace_launcher(
    argv: &[String],
    method: &str,
    params: Option<&Value>,
    code: Option<&str>,
    parse_error: Option<&str>,
) {
    use vk_server::compat::trace;
    if trace::settings().is_none() {
        return;
    }
    let Ok(plugin) = std::env::var("HERDR_PLUGIN_ID") else {
        return;
    };
    let invocation = std::env::var(trace::INVOCATION_ENV).ok();
    trace::write(&trace::Line {
        plugin: &plugin,
        invocation: invocation.as_deref(),
        transport: "launcher",
        method,
        ok: code.is_none(),
        code,
        params,
        argv: Some(argv),
        parse_error,
    });
}

/// The method name a launcher trace line records for a registry operation.
fn local_method(op: &Local) -> &'static str {
    match op {
        Local::PluginInstall { .. } => "plugin.install",
        Local::PluginLink { .. } => "plugin.link",
        Local::PluginUnlink { .. } => "plugin.unlink",
        Local::PluginUninstall { .. } => "plugin.uninstall",
        Local::PluginEnable { .. } => "plugin.enable",
        Local::PluginDisable { .. } => "plugin.disable",
        Local::PluginList { .. } => "plugin.list",
        Local::PluginConfigDir { .. } => "plugin.config_dir",
        Local::PluginUpdate { .. } => "plugin.update",
    }
}

/// Server lifecycle methods are never forwarded by the shim (to Vibeke or anything else), except
/// `server.stop`, which only ever reaches Vibeke's own session and is refused by the server
/// unless `[compat.herdr] allow_server_stop` is set (and always from a plugin or a pane).
fn lifecycle(method: &str) -> bool {
    method.starts_with("server.") && !matches!(method, "server.reload_config" | "server.stop")
}

/// A single-use ticket from our own broker that proves this invocation to another session.
async fn invocation_ticket(broker: &Path) -> Result<String, String> {
    let s = tokio::net::UnixStream::connect(broker)
        .await
        .map_err(|e| e.to_string())?;
    let (rd, mut wr) = s.into_split();
    let line = request_line(broker, "t", "vibeke.invocation_ticket", json!({}));
    wr.write_all(line.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let resp = BufReader::new(rd)
        .lines()
        .next_line()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no response")?;
    let v: Value = serde_json::from_str(&resp).map_err(|e| e.to_string())?;
    v["result"]["ticket"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| {
            v["error"]["message"]
                .as_str()
                .unwrap_or("no ticket")
                .to_string()
        })
}

/// Herdr prints a response it parsed into a `serde_json::Value` built without `preserve_order`,
/// so its object keys come out sorted. Vibeke keeps insertion order; sort them the same way.
fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sorted(&m[k])))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        x => x.clone(),
    }
}

/// An error line in Herdr's `ErrorResponse` shape (`id` first, as Herdr serializes it).
fn herdr_error(id: &str, code: &str, msg: impl std::fmt::Display) -> Value {
    json!({"id": id, "error": {"code": code, "message": msg.to_string()}})
}

/// Herdr's `server_not_running` error for a socket nothing answers on (by default the
/// session's Herdr-style listener path).
fn not_running(id: &str, session: &str) -> Value {
    not_running_at(id, &vk_server::compat::listener_for(session), session)
}

fn not_running_at(id: &str, socket: &Path, session: &str) -> Value {
    let attach = if session == herdr::DEFAULT_SESSION {
        "herdr".to_string()
    } else {
        format!("herdr session attach {session}")
    };
    herdr_error(
        id,
        "server_not_running",
        format!(
            "no herdr server is running at {}; run `{attach}` to start or attach it",
            socket.display()
        ),
    )
}

/// A native call error in Herdr's shape.
fn call_error(id: &str, e: &client::CallError) -> Value {
    match e {
        client::CallError::Rpc(r) => herdr_error(id, &r.data.kind, &r.message),
        client::CallError::Io(err) => herdr_error(id, "io_error", format!("{err:#}")),
    }
}

/// Print the answer to a request the way Herdr's printer for that command does: errors as the
/// whole response line on stderr (exit 1), success per `output`.
fn print_answer(resp: &Value, output: Output) -> i32 {
    use std::io::Write;
    if resp.get("error").is_some() {
        eprintln!("{}", sorted(resp));
        return 1;
    }
    match output {
        Output::Response => println!("{}", sorted(resp)),
        Output::Quiet => {}
        Output::ReadText => {
            if let Some(t) = resp["result"]["read"]["text"].as_str() {
                print!("{t}");
            }
        }
        Output::Explain { json, verbose } => {
            let e = &resp["result"]["explain"];
            if json {
                println!("{}", sorted(e));
            } else {
                print!("{}", herdr::cli::explain_text(e, verbose));
            }
        }
    }
    let _ = std::io::stdout().flush();
    EXIT_OK
}

/// One request over a Herdr socket (the invocation's broker): the response line Herdr's wire
/// returns, which echoes the request `id`.
async fn raw(
    sock: &Path,
    id: &str,
    method: &str,
    params: Value,
    session: &str,
) -> Result<Value, Value> {
    let stream = tokio::net::UnixStream::connect(sock)
        .await
        .map_err(|_| not_running_at(id, sock, session))?;
    let (rd, mut wr) = stream.into_split();
    let line = request_line(sock, id, method, params);
    wr.write_all(line.as_bytes())
        .await
        .map_err(|e| herdr_error(id, "io_error", e))?;
    let first = BufReader::new(rd)
        .lines()
        .next_line()
        .await
        .ok()
        .flatten()
        .ok_or_else(|| {
            herdr_error(
                id,
                "io_error",
                "the server closed the connection without a response",
            )
        })?;
    serde_json::from_str(&first)
        .map_err(|e| herdr_error(id, "invalid_response", format!("{e}: {first}")))
}

/// One request through a session's native socket (`compat.herdr.call`), answered as Herdr's
/// response line with `id`.
async fn over_native(
    stream: tokio::net::UnixStream,
    id: &str,
    method: &str,
    params: Value,
    as_plugin: Option<Value>,
) -> Result<Value, Value> {
    let mut c = Client::new(stream);
    c.hello("herdr-shim")
        .await
        .map_err(|e| call_error(id, &e))?;
    let mut p = json!({"method": method, "params": params});
    if let Some(ap) = as_plugin {
        p["as_plugin"] = ap;
    }
    let mut v = c
        .call("compat.herdr.call", p)
        .await
        .map_err(|e| call_error(id, &e))?;
    if let Some(m) = v.as_object_mut() {
        m.insert("id".into(), json!(id));
    }
    Ok(v)
}

/// Send one request where the shim's routing rules send it and return the response line, or
/// an error line in Herdr's shape.
///
/// Session selection (07 §8.2): Herdr's global `--session NAME`, then `HERDR_SESSION`, then
/// the invocation's own session (its broker's, else `VIBEKE_SESSION`/`--session` of vibeke).
/// Selecting another session from a pane is refused; from a plugin invocation the destination
/// re-checks the plugin's grant. An explicitly selected session is never spawned, and neither
/// is any session when `spawn` is false.
async fn send(
    g: &Global,
    selected: Option<String>,
    id: &str,
    method: &str,
    params: Value,
    spawn: bool,
) -> Result<Value, Value> {
    if lifecycle(method) {
        return Err(herdr_error(
            id,
            "unsupported",
            format!(
                "{method} is not forwarded by the Herdr compatibility shim; use `vibeke server …`"
            ),
        ));
    }
    let broker = broker();
    // Inside a plugin invocation whose broker is gone (the action ended, the grant was
    // revoked): never fall back to the user-level socket, which would bypass the grant.
    if in_plugin() && broker.is_none() {
        return Err(herdr_error(
            id,
            "permission_denied",
            "this plugin invocation's broker is closed (the invocation ended or its grant was revoked)",
        ));
    }
    let current = broker
        .as_ref()
        .map(|(_, s)| s.clone())
        .unwrap_or_else(|| g.session.clone());
    let target = selected.unwrap_or_else(|| current.clone());
    if target != current {
        // Any process inside a pane of any session (token or not) stays in its session; the
        // destination enforces the same rule.
        if pane_scoped()
            || vk_server::run::inside_any_pane(
                Some(std::process::id() as i32),
                &vk_server::paths::runtime_root(),
            )
        {
            return Err(herdr_error(
                id,
                "permission_denied",
                "a pane cannot select another session",
            ));
        }
        // The plugin's identity travels as a ticket from its own broker, never as an
        // environment value.
        let as_plugin = match &broker {
            Some((b, session)) => match invocation_ticket(b).await {
                Ok(t) => Some(json!({"session": session, "ticket": t})),
                Err(e) => {
                    return Err(herdr_error(
                        id,
                        "permission_denied",
                        format!("this plugin invocation cannot prove its identity: {e}"),
                    ));
                }
            },
            None => None,
        };
        let socket = vk_server::paths::Paths::new(&target).socket();
        let stream = client::connect(&socket)
            .await
            .map_err(|_| not_running(id, &target))?;
        return over_native(stream, id, method, params, as_plugin).await;
    }
    if let Some((b, session)) = broker {
        return raw(&b, id, method, params, &session).await;
    }
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    let stream = if spawn {
        client::connect_or_spawn(&g.session, &socket, g.no_spawn).await
    } else {
        client::connect(&socket).await
    };
    let stream = stream.map_err(|_| not_running(id, &g.session))?;
    over_native(stream, id, method, params, None).await
}

/// `herdr session list [--json]`: Vibeke's sessions in Herdr's shape. A session is a state
/// directory with a database; `default` is always listed first.
fn session_list(json_out: bool) -> i32 {
    let root = vk_server::paths::state_root();
    let mut names: Vec<String> = std::fs::read_dir(&root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("state.db").exists())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n != herdr::DEFAULT_SESSION && herdr::valid_session_name(n))
        .collect();
    names.sort();
    names.insert(0, herdr::DEFAULT_SESSION.to_string());
    let rows: Vec<Value> = names
        .iter()
        .map(|n| {
            let socket = vk_server::paths::Paths::new(n).socket();
            json!({
                "name": n,
                "default": n == herdr::DEFAULT_SESSION,
                "running": std::os::unix::net::UnixStream::connect(&socket).is_ok(),
                "socket_path": vk_server::compat::listener_for(n).display().to_string(),
                "session_dir": root.join(n).display().to_string(),
            })
        })
        .collect();
    if json_out {
        println!("{}", sorted(&json!({"sessions": rows})));
        return EXIT_OK;
    }
    println!("{:<20} {:<8} {:<48} socket", "name", "status", "directory");
    for r in &rows {
        println!(
            "{:<20} {:<8} {:<48} {}",
            r["name"].as_str().unwrap_or(""),
            if r["running"] == true {
                "running"
            } else {
                "stopped"
            },
            r["session_dir"].as_str().unwrap_or(""),
            r["socket_path"].as_str().unwrap_or("")
        );
    }
    EXIT_OK
}

fn yes_no(b: Option<bool>) -> &'static str {
    match b {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

/// `herdr status [server|client] [--json]` (Herdr's `src/cli/status.rs`). The client is the
/// emulated Herdr 0.9.3; the server is whatever answers `ping` on the shim's route (it is never
/// spawned for this).
async fn status(g: &Global, selected: Option<String>, scope: StatusScope, json_out: bool) -> i32 {
    let session = selected
        .clone()
        .or_else(|| broker().map(|(_, s)| s))
        .unwrap_or_else(|| g.session.clone());
    let socket = std::env::var_os("HERDR_SOCKET_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| vk_server::compat::listener_for(&session))
        .display()
        .to_string();
    let named = (session != herdr::DEFAULT_SESSION).then(|| session.clone());
    let version = herdr::BASELINE_VERSION;
    let channel = herdr::cli_config::update_channel();
    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("unknown ({e})"));
    let client_json = json!({
        "version": version,
        "channel": channel,
        "protocol": herdr::cli::PROTOCOL_VERSION,
        "endpoint_protocol_generation": herdr::cli::ENDPOINT_PROTOCOL_GENERATION,
        "endpoint_capabilities": ["surface_interest", "presentation_effects_fence", "health_check"],
        "remote_host_bridge": true,
        "remote_bridge_idle_timeout": true,
        "binary": binary,
        "session": named,
    });
    if scope == StatusScope::Client {
        if json_out {
            println!("{client_json}");
        } else {
            println!("version: {version}");
            println!("channel: {channel}");
            println!("protocol: {}", herdr::cli::PROTOCOL_VERSION);
            println!(
                "endpoint_protocol_generation: {}",
                herdr::cli::ENDPOINT_PROTOCOL_GENERATION
            );
            println!("binary: {binary}");
        }
        return EXIT_OK;
    }
    let ping = match send(g, selected, "api-client:status", "ping", json!({}), false).await {
        Ok(v) if v.get("error").is_none() => Some(v["result"].clone()),
        _ => None,
    };
    let running = ping.is_some();
    let ping = ping.unwrap_or(Value::Null);
    let server_version = ping["version"].as_str().map(str::to_string);
    let protocol = ping["protocol"].as_u64();
    let caps = ping.get("capabilities").filter(|c| c.is_object()).cloned();
    let generation = caps
        .as_ref()
        .and_then(|c| c["endpoint_protocol_generation"].as_u64());
    let compatible = protocol.map(|p| p == u64::from(herdr::cli::PROTOCOL_VERSION));
    let endpoint_compatible =
        generation.map(|n| n == u64::from(herdr::cli::ENDPOINT_PROTOCOL_GENERATION));
    let (restart_needed, stale) = if running {
        (
            Some(generation != Some(u64::from(herdr::cli::ENDPOINT_PROTOCOL_GENERATION))),
            server_version.as_deref().map(|v| v != version),
        )
    } else {
        (Some(false), Some(false))
    };
    let server_json = json!({
        "status": if running { "running" } else { "not_running" },
        "running": running,
        "version": server_version,
        "protocol": protocol,
        "capabilities": caps.as_ref().map(|c| {
            let b = |k: &str| c[k].as_bool().unwrap_or(false);
            json!({
                "live_handoff": b("live_handoff"),
                "detached_server_daemon": b("detached_server_daemon"),
                "endpoint_protocol_generation": generation,
                "surface_interest": b("surface_interest"),
                "health_check": b("health_check"),
                "ssh_agent_registration": b("ssh_agent_registration"),
            })
        }),
        "compatible": compatible,
        "endpoint_compatible": endpoint_compatible,
        "socket": socket,
        "session": named,
        "restart_needed": restart_needed,
        "server_binary_stale": stale,
    });
    let server_body = |indent: &str| {
        if running {
            println!("{indent}status: running");
            println!(
                "{indent}version: {}",
                server_version.as_deref().unwrap_or("unknown")
            );
            println!(
                "{indent}endpoint_compatible: {}",
                yes_no(endpoint_compatible)
            );
            println!(
                "{indent}private_protocol: {}",
                protocol.map_or("unknown".to_string(), |p| p.to_string())
            );
            println!(
                "{indent}private_protocol_compatible: {}",
                yes_no(compatible)
            );
        } else {
            println!("{indent}status: not running");
        }
        println!("{indent}socket: {socket}");
    };
    match (scope, json_out) {
        (StatusScope::Server, true) => println!("{server_json}"),
        (StatusScope::Server, false) => server_body(""),
        (_, true) => println!(
            "{}",
            json!({
                "client": client_json,
                "server": server_json,
                "update": {"restart_needed": restart_needed, "server_binary_stale": stale},
            })
        ),
        (_, false) => {
            println!("client:");
            println!("  version: {version}");
            println!("  channel: {channel}");
            println!("  protocol: {}", herdr::cli::PROTOCOL_VERSION);
            println!(
                "  endpoint_protocol_generation: {}",
                herdr::cli::ENDPOINT_PROTOCOL_GENERATION
            );
            println!();
            println!("server:");
            server_body("  ");
            println!();
            println!("update:");
            println!("  restart_needed: {}", yes_no(restart_needed));
            println!("  server_binary_stale: {}", yes_no(stale));
        }
    }
    EXIT_OK
}

/// `vibeke compat herdr <args>` / `herdr <args>`: Herdr 0.9.3's CLI, answered by Vibeke.
pub async fn herdr_shim(g: &Global, args: &[String]) -> i32 {
    use std::io::Write;
    let argv = args;
    let (selected, args) = match herdr::cli::take_session(args) {
        Ok(x) => x,
        Err(e) => {
            trace_launcher(argv, "", None, Some("usage"), Some(e.as_str()));
            eprintln!("error: {e}\nrun 'herdr --help' for usage");
            return EXIT_USAGE;
        }
    };
    let selected = selected.or_else(|| {
        std::env::var("HERDR_SESSION")
            .ok()
            .filter(|s| herdr::valid_session_name(s))
    });
    let parsed = herdr::cli::parse(&args);
    match &parsed {
        Parsed::Call { method, params, .. } => {
            trace_launcher(argv, method, Some(params), None, None)
        }
        Parsed::Local(op) => trace_launcher(argv, local_method(op), None, None, None),
        Parsed::Version => trace_launcher(argv, "version", None, None, None),
        Parsed::Help(_) | Parsed::Usage { code: 0, .. } => {
            trace_launcher(argv, "help", None, None, None)
        }
        Parsed::Usage { text, .. } => {
            trace_launcher(argv, "", None, Some("usage"), Some(text.as_str()))
        }
        Parsed::Failed(m) => trace_launcher(argv, "", None, Some("usage"), Some(m.as_str())),
        Parsed::Refused { command, .. } => {
            trace_launcher(argv, command, None, Some("unsupported"), None)
        }
        Parsed::ConfigCheck => trace_launcher(argv, "config.check", None, None, None),
        Parsed::SessionList { .. } => trace_launcher(argv, "session.list", None, None, None),
        Parsed::Status { .. } => trace_launcher(argv, "status", None, None, None),
        Parsed::ApiSchema(_) => trace_launcher(argv, "api.schema", None, None, None),
    }
    match parsed {
        Parsed::Version => {
            // Exactly Herdr's line: plugins parse it for version checks. `herdr status`
            // reports that Vibeke serves the shim.
            println!("herdr {}", herdr::BASELINE_VERSION);
            EXIT_OK
        }
        Parsed::Help(h) => {
            print!("{h}");
            let _ = std::io::stdout().flush();
            EXIT_OK
        }
        Parsed::Usage { text, code } => {
            eprintln!("{text}");
            code
        }
        Parsed::Failed(m) => {
            // What Herdr's `main` prints for the `io::Error` it returns.
            eprintln!("Error: Custom {{ kind: Other, error: {m:?} }}");
            1
        }
        Parsed::Refused { command, reason } => {
            eprintln!(
                "{}",
                herdr_error(
                    "cli:request",
                    "unsupported",
                    format!("herdr {command}: {reason}")
                )
            );
            1
        }
        Parsed::ConfigCheck => {
            // Local, like Herdr: no server, and allowed inside a plugin invocation.
            let path = herdr::cli_config::config_path();
            let (out, code) = herdr::cli_config::report(&herdr::cli_config::check_file(&path));
            print!("{out}");
            let _ = std::io::stdout().flush();
            code
        }
        Parsed::SessionList { json } => session_list(json),
        Parsed::Status { scope, json } => status(g, selected, scope, json).await,
        Parsed::ApiSchema(s) => {
            match s {
                ApiSchema::Summary => print!("{}", herdr::cli::api_schema_summary()),
                ApiSchema::Json => print!("{}", herdr::cli::API_SCHEMA_JSON),
                ApiSchema::Output(path) => {
                    if let Err(e) = std::fs::write(&path, herdr::cli::API_SCHEMA_JSON) {
                        eprintln!("Error: {e:?}");
                        return 1;
                    }
                    println!("wrote API schema to {path}");
                }
            }
            let _ = std::io::stdout().flush();
            EXIT_OK
        }
        Parsed::Local(op) => {
            // The Herdr CLI prints JSON; so does the shim. The registry is per user, shared by
            // every session, so session selection does not change it.
            let g = Global {
                json: Some(true),
                ..g.clone()
            };
            let mutating = !matches!(op, Local::PluginList { .. } | Local::PluginConfigDir { .. });
            // The shim in a plugin invocation can never accept trust on the user's behalf.
            let code = herdr_local(&g, op);
            if mutating && code == EXIT_OK {
                notify_registry(&g).await;
            }
            code
        }
        Parsed::Call {
            id,
            method,
            params,
            output,
        } => match send(g, selected, &id, &method, params, true).await {
            Ok(v) => print_answer(&v, output),
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
    }
}

/// The id Herdr's CLI gives every `herdr plugin …` request.
const PLUGIN_CLI_ID: &str = "cli:plugin";

/// A Herdr error line for a `herdr plugin …` command on stderr (exit 1).
fn plugin_cli_error(code: &str, msg: impl std::fmt::Display) -> i32 {
    eprintln!("{}", sorted(&herdr_error(PLUGIN_CLI_ID, code, msg)));
    1
}

/// Herdr's `source_display` for the human `plugin list`.
fn plugin_source_display(p: &Value) -> String {
    let src = &p["source"];
    if src["kind"] == "github" {
        let s = |k: &str| src[k].as_str().unwrap_or("unknown").to_string();
        let subdir = src["subdir"]
            .as_str()
            .map(|d| format!("/{d}"))
            .unwrap_or_default();
        let reference = src["requested_ref"]
            .as_str()
            .or(src["resolved_commit"].as_str())
            .unwrap_or("unknown");
        format!("github:{}/{}{subdir}@{reference}", s("owner"), s("repo"))
    } else {
        format!("local:{}", p["plugin_root"].as_str().unwrap_or(""))
    }
}

/// Herdr's `print_install_preview` (written to stderr), from the `InstalledPluginInfo` of the
/// plugin being installed and of the one it replaces.
fn install_preview(p: &Value, replaces: Option<&Value>) -> String {
    let list = |k: &str| p[k].as_array().cloned().unwrap_or_default();
    let cmd = |v: &Value| {
        v["command"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    };
    let text = |v: &Value| v.as_str().unwrap_or("").to_string();
    let mut o = String::from("Plugin install preview:\n");
    o.push_str(&format!("  id: {}\n", text(&p["plugin_id"])));
    o.push_str(&format!("  name: {}\n", text(&p["name"])));
    o.push_str(&format!("  version: {}\n", text(&p["version"])));
    let src = &p["source"];
    if let (Some(owner), Some(repo)) = (src["owner"].as_str(), src["repo"].as_str()) {
        let subdir = src["subdir"]
            .as_str()
            .map(|d| format!("/{d}"))
            .unwrap_or_default();
        o.push_str(&format!("  source: {owner}/{repo}{subdir}\n"));
    }
    if let Some(r) = src["requested_ref"].as_str() {
        o.push_str(&format!("  ref: {r}\n"));
    }
    if let Some(c) = src["resolved_commit"].as_str() {
        o.push_str(&format!("  commit: {c}\n"));
    }
    for (label, k) in [
        ("actions", "actions"),
        ("startup commands", "startup"),
        ("events", "events"),
        ("panes", "panes"),
        ("link handlers", "link_handlers"),
        ("build commands", "build"),
    ] {
        o.push_str(&format!("  {label}: {}\n", list(k).len()));
    }
    let pf = herdr::current_platform();
    for b in list("build") {
        let platforms = b
            .get("platforms")
            .filter(|x| x.is_array())
            .or_else(|| p.get("platforms").filter(|x| x.is_array()));
        let supported = platforms.is_none_or(|x| {
            x.as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(pf)))
        });
        let skip = if supported {
            String::new()
        } else {
            format!(" (skipped on {pf})")
        };
        o.push_str(&format!("    build{skip}: {}\n", cmd(&b)));
    }
    for x in list("startup") {
        o.push_str(&format!("    startup: {}\n", cmd(&x)));
    }
    for x in list("actions") {
        o.push_str(&format!("    action {}: {}\n", text(&x["id"]), cmd(&x)));
    }
    for x in list("events") {
        o.push_str(&format!("    event {}: {}\n", text(&x["on"]), cmd(&x)));
    }
    for x in list("panes") {
        o.push_str(&format!("    pane {}: {}\n", text(&x["id"]), cmd(&x)));
    }
    for w in list("warnings") {
        o.push_str(&format!("  warning: {}\n", text(&w)));
    }
    if let Some(e) = replaces {
        o.push_str(&format!(
            "  replaces: {} from {}\n",
            text(&e["plugin_id"]),
            plugin_source_display(e)
        ));
    }
    o
}

/// Herdr's human `plugin list` (`print_plugin_list_human`).
fn plugin_list_human(plugins: &[Value], dirs: &registry::PluginDirs) -> String {
    if plugins.is_empty() {
        return "No plugins installed.\n".into();
    }
    let mut out = format!(
        "{} plugin{} installed:\n",
        plugins.len(),
        if plugins.len() == 1 { "" } else { "s" }
    );
    for p in plugins {
        let id = p["plugin_id"].as_str().unwrap_or("");
        let warnings: Vec<&str> = p["warnings"]
            .as_array()
            .map(|w| w.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let warning = if warnings.is_empty() {
            String::new()
        } else {
            format!("; {} warning(s)", warnings.len())
        };
        out.push_str(&format!(
            "- {} ({}) {} [{}{}]\n",
            id,
            p["name"].as_str().unwrap_or(id),
            if p["enabled"] == true {
                "enabled"
            } else {
                "disabled"
            },
            plugin_source_display(p),
            warning
        ));
        out.push_str(&format!("  config: {}\n", dirs.config_dir(id).display()));
        for w in warnings {
            out.push_str(&format!("  warning: {w}\n"));
        }
    }
    out
}

/// `herdr plugin …` registry commands as Herdr 0.9.3 prints them: the response line
/// (`{"id":"cli:plugin","result":…}`) for `link`, `unlink`, `enable`, `disable` and
/// `list --json`; Herdr's summary for `list`; the bare directory for `config-dir`; Herdr's
/// lines for `install` and `uninstall`. Herdr's CLI changes the registry file itself when no
/// server runs; the shim always does, and tells the server afterwards. Errors are Herdr error
/// lines on stderr (exit 1).
fn herdr_local(g: &Global, op: Local) -> i32 {
    use std::io::Write;
    let dirs = vk_server::compat::plugin_dirs();
    let mutating = !matches!(op, Local::PluginList { .. } | Local::PluginConfigDir { .. });
    if mutating && pane_scoped() {
        return plugin_cli_error(
            "permission_denied",
            "plugin registry changes are not allowed from a pane; ask the user",
        );
    }
    let respond = |method: &str, params: Value| -> i32 {
        match vk_server::compat::herdr_registry_op(method, &params) {
            Ok(result) => {
                println!(
                    "{}",
                    sorted(&json!({"id": PLUGIN_CLI_ID, "result": result}))
                );
                EXIT_OK
            }
            Err(e) => plugin_cli_error(&e.code, e.message),
        }
    };
    let code = match op {
        Local::PluginLink { path, enabled, .. } => {
            // Herdr sends an absolute path (`~` expanded, relative to the working directory).
            let home = || PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
            let p = match path.strip_prefix("~/") {
                Some(rest) => home().join(rest),
                None if path == "~" => home(),
                None => PathBuf::from(&path),
            };
            let p = if p.is_absolute() {
                p
            } else {
                std::env::current_dir().unwrap_or_default().join(p)
            };
            respond(
                "plugin.link",
                json!({"path": p.display().to_string(), "enabled": enabled}),
            )
        }
        Local::PluginUnlink { id } => respond("plugin.unlink", json!({"plugin_id": id})),
        Local::PluginEnable { id } => respond("plugin.enable", json!({"plugin_id": id})),
        Local::PluginDisable { id } => respond("plugin.disable", json!({"plugin_id": id})),
        Local::PluginList { plugin, json } => {
            let r = match vk_server::compat::herdr_registry_op(
                "plugin.list",
                &json!({"plugin_id": plugin}),
            ) {
                Ok(r) => r,
                Err(e) => return plugin_cli_error(&e.code, e.message),
            };
            if json {
                println!("{}", sorted(&json!({"id": PLUGIN_CLI_ID, "result": r})));
            } else {
                let plugins = r["plugins"].as_array().cloned().unwrap_or_default();
                print!("{}", plugin_list_human(&plugins, &dirs));
            }
            EXIT_OK
        }
        Local::PluginConfigDir { id } => {
            // Any id: the directory name is Herdr's encoding of it (`path_component`).
            let d = dirs.config_dir(&id);
            let made = std::fs::create_dir_all(&d)
                .and_then(|_| std::fs::create_dir_all(dirs.state_dir(&id)));
            if let Err(e) = made {
                eprintln!("Error: {e:?}");
                return 1;
            }
            println!("{}", d.display());
            EXIT_OK
        }
        Local::PluginUninstall { id: target } => {
            let reg = match Registry::load(&dirs) {
                Ok(r) => r,
                Err(e) => return plugin_cli_error("plugin_registry_load_failed", e),
            };
            // A plugin id, or the `owner/repo[/subdir]` it was installed from.
            let entry = reg.get(&target).ok().cloned().or_else(|| {
                reg.plugins
                    .values()
                    .find(|e| e.origin.repo.as_deref() == Some(target.as_str()))
                    .cloned()
            });
            let Some(entry) = entry else {
                eprintln!("plugin not installed: {target}");
                return 1;
            };
            let r = if entry.managed {
                Registry::update(&dirs, |r| r.uninstall(&dirs, &entry.id))
            } else {
                Registry::update(&dirs, |r| r.unlink(&entry.id))
            };
            match r {
                Ok(_) => {
                    println!("Uninstalled {}.", entry.id);
                    EXIT_OK
                }
                Err(RegistryError::NotFound(_)) => {
                    eprintln!("plugin not installed: {target}");
                    1
                }
                Err(e) => plugin_cli_error("plugin_uninstall_failed", e),
            }
        }
        Local::PluginInstall {
            source,
            git_ref,
            yes,
        } => {
            if !yes && !std::io::stdin().is_terminal() {
                eprintln!("remote plugin install requires --yes when stdin is not interactive");
                return 2;
            }
            let before = Registry::load(&dirs).ok();
            let before_info = vk_server::compat::herdr_plugin_list(None);
            let quiet = Global {
                quiet: true,
                ..g.clone()
            };
            let code = local(
                &quiet,
                Local::PluginInstall {
                    source: source.clone(),
                    git_ref,
                    yes,
                },
            );
            if code != EXIT_OK {
                return code;
            }
            // The registration this install wrote: the newest one.
            let installed = Registry::load(&dirs).ok().and_then(|r| {
                r.plugins
                    .values()
                    .filter(|e| {
                        before.as_ref().is_none_or(|b| {
                            b.get(&e.id)
                                .map(|o| o.installed_at_ms != e.installed_at_ms)
                                .unwrap_or(true)
                        })
                    })
                    .max_by_key(|e| e.installed_at_ms)
                    .cloned()
            });
            if let Some(e) = installed {
                // Herdr's install preview on stderr, then its confirmation without `--yes`
                // (declining removes the registration again).
                let replaces = before_info.iter().find(|x| x["plugin_id"] == e.id.as_str());
                if let Some(info) = vk_server::compat::herdr_plugin_list(Some(&e.id)).first() {
                    eprint!("{}", install_preview(info, replaces));
                }
                // A replacement cannot be undone here (the previous checkout is gone), so only
                // a new plugin is confirmed; either way it stays inactive until trusted.
                if !yes && replaces.is_none() {
                    eprint!("Install this plugin? [y/N] ");
                    let _ = std::io::stderr().flush();
                    let mut line = String::new();
                    let _ = std::io::stdin().read_line(&mut line);
                    if !matches!(line.trim(), "y" | "Y" | "yes" | "YES" | "Yes") {
                        let _ = Registry::update(&dirs, |r| r.uninstall(&dirs, &e.id));
                        eprintln!("plugin install cancelled");
                        return EXIT_OK;
                    }
                }
                println!("Installed {} from {}.", e.id, source);
                println!("Config: {}", dirs.config_dir(&e.id).display());
                if !yes {
                    eprintln!(
                        "{} is inactive until you trust it: vibeke plugin trust {} --legacy",
                        e.id, e.id
                    );
                }
            }
            EXIT_OK
        }
        other => local(g, other),
    };
    let _ = std::io::stdout().flush();
    code
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_preview_prints_like_herdr() {
        let p = json!({
            "plugin_id": "acme.x", "name": "X", "version": "1.2.0",
            "source": {"kind": "github", "owner": "acme", "repo": "tools", "subdir": "x", "requested_ref": "v1", "resolved_commit": "abc"},
            "build": [{"command": ["make"]}, {"command": ["nmake"], "platforms": ["windows"]}],
            "actions": [{"id": "go", "command": ["sh", "-c", "true"]}],
            "events": [{"on": "pane.created", "command": ["hook"]}],
            "warnings": ["w1"],
        });
        let old =
            json!({"plugin_id": "acme.x", "plugin_root": "/old", "source": {"kind": "local"}});
        assert_eq!(
            install_preview(&p, Some(&old)),
            "Plugin install preview:\n  id: acme.x\n  name: X\n  version: 1.2.0\n  source: acme/tools/x\n  ref: v1\n  commit: abc\n  actions: 1\n  startup commands: 0\n  events: 1\n  panes: 0\n  link handlers: 0\n  build commands: 2\n    build: make\n    build (skipped on "
                .to_string()
                + herdr::current_platform()
                + "): nmake\n    action go: sh -c true\n    event pane.created: hook\n  warning: w1\n  replaces: acme.x from local:/old\n"
        );
    }

    #[test]
    fn response_lines_print_like_herdr() {
        // Herdr re-serializes the response it parsed: object keys sorted at every level.
        let line = json!({"result": {"type": "pane_info", "pane": {"pane_id": "w1:p1", "focused": true}}, "id": "cli:pane:get"});
        assert_eq!(
            sorted(&line).to_string(),
            r#"{"id":"cli:pane:get","result":{"pane":{"focused":true,"pane_id":"w1:p1"},"type":"pane_info"}}"#
        );
        let err =
            json!({"id": "cli:pane:get", "error": {"message": "w9:p9", "code": "pane_not_found"}});
        assert_eq!(
            sorted(&err).to_string(),
            r#"{"error":{"code":"pane_not_found","message":"w9:p9"},"id":"cli:pane:get"}"#
        );
        assert_eq!(
            sorted(&json!([{"b": 1, "a": 2}])).to_string(),
            r#"[{"a":2,"b":1}]"#
        );
    }

    #[test]
    fn shim_errors_use_herdrs_error_response() {
        // The shim's own errors are Herdr's `ErrorResponse` struct: `id` first.
        assert_eq!(
            herdr_error("cli:pane:list", "permission_denied", "no").to_string(),
            r#"{"id":"cli:pane:list","error":{"code":"permission_denied","message":"no"}}"#
        );
        let v = not_running("cli:pane:get", "default");
        assert_eq!(v["error"]["code"], "server_not_running");
        let m = v["error"]["message"].as_str().unwrap();
        assert!(m.starts_with("no herdr server is running at "), "{m}");
        assert!(m.ends_with("; run `herdr` to start or attach it"), "{m}");
        let named = not_running("cli:pane:get", "work");
        assert!(
            named["error"]["message"]
                .as_str()
                .unwrap()
                .ends_with("run `herdr session attach work` to start or attach it")
        );
    }

    #[test]
    fn printers_follow_the_command() {
        let ok = json!({"id": "cli:request", "result": {"type": "ok"}});
        assert_eq!(print_answer(&ok, Output::Response), 0);
        assert_eq!(print_answer(&ok, Output::Quiet), 0);
        let err = json!({"id": "cli:request", "error": {"code": "pane_not_found", "message": "x"}});
        // Every error exits 1, `permission_denied` included.
        assert_eq!(print_answer(&err, Output::Quiet), 1);
        let denied =
            json!({"id": "cli:plugin", "error": {"code": "permission_denied", "message": "x"}});
        assert_eq!(print_answer(&denied, Output::Response), 1);
    }
}
