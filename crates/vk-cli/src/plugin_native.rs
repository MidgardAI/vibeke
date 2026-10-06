//! `vibeke plugin …` for native plugins (07 §7.6, 09 §6; lane 3B). Hooked in front of the
//! Herdr plugin verbs: a verb is handled here when its source has a `vibeke-plugin.toml` or its
//! id names a native registration (and for the native-only verbs `consent`, `search`,
//! `export`, `import`, `restart`, `update --auto`, `logs -f`); everything else falls through.
//!
//! * `install <dir | manifest | owner/repo[@ref]> [--ref R] [--yes] [--accept a,b] [--dry-run]`
//!   shows source/commit, entrypoints and every requested capability with its risk, asks for
//!   consent (`--yes` / `--accept` answer non-interactively), runs `[[build]]` and enables.
//! * `update <id> [--yes]` re-fetches a repository install and shows the capability *changes*;
//!   a widening update stays inactive (`reconsent`) until `plugin consent <id>`. `update --auto`
//!   applies only same-major.minor, non-widening updates (the opt-in `auto_update = "patch"`).
//! * `link <dir> [--yes]` registers a dev directory (hot restart by the server); `consent <id>`
//!   re-consents; `search <q> [--index URL]` reads the marketplace index (post-1.0 hosting;
//!   `[plugins] index_url`, `file://` mirrors); `logs <id> -f` follows the log;
//!   `export <dir>` / `import <dir> [--dry-run]` back up the registry and plugin files.
//!
//! Registry changes are refused from panes (`VIBEKE_PANE_TOKEN`) and from plugin invocations
//! (`VIBEKE_PLUGIN_TOKEN`).

use crate::client::{self, Client};
use crate::{EXIT_API, EXIT_NO_SERVER, EXIT_OK, EXIT_PERMISSION, EXIT_USAGE, Global};
use serde_json::{Value, json};
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use vk_compat::herdr::registry::{self as hreg, Registry, RegistryError};
use vk_compat::herdr::source;
use vk_compat::native::registry::{
    self as nreg, NativeStatus, consent_and_build, consent_terms, native_status,
};
use vk_compat::native::{backup, index};

pub const HELP: &str = "Native plugins (vibeke-plugin.toml):
  vibeke plugin install <dir | owner/repo[@ref]> [--accept a,b | --yes] [--dry-run]
  vibeke plugin update <id> [--yes] | update --auto
  vibeke plugin consent <id> [--yes]     approve the requested capabilities again
  vibeke plugin link <dir> [--yes]       dev mode: the server hot-restarts on change
  vibeke plugin restart <id>
  vibeke plugin logs <id> -f
  vibeke plugin search <query> [--index URL]
  vibeke plugin export <dir> | import <dir> [--dry-run]";

fn json_out(g: &Global) -> bool {
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
        RegistryError::Conflict(m) if m.starts_with("capabilities_not_accepted") => {
            fail("permission_denied", m, EXIT_PERMISSION)
        }
        RegistryError::Conflict(m) => fail("conflict", m, EXIT_API),
        other => fail("internal", other, EXIT_API),
    }
}

fn print(g: &Global, v: &Value, human: impl FnOnce() -> String) {
    if g.quiet {
        return;
    }
    if json_out(g) {
        println!("{v}");
    } else {
        println!("{}", human());
    }
}

fn flag(args: &[String], n: &str) -> bool {
    args.iter().any(|a| a == n)
}

fn value(args: &[String], n: &str) -> Option<String> {
    args.iter()
        .position(|a| a == n)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn positionals(args: &[String]) -> Vec<String> {
    let valued = ["--ref", "--accept", "--index", "--limit"];
    let mut out = vec![];
    let mut i = 0;
    while i < args.len() {
        if valued.contains(&args[i].as_str()) {
            i += 2;
            continue;
        }
        if !args[i].starts_with('-') {
            out.push(args[i].clone());
        }
        i += 1;
    }
    out
}

/// Registry changes are operator decisions: never from a pane or a plugin.
fn refused() -> Option<i32> {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    if set("VIBEKE_PANE_TOKEN") || set("VIBEKE_PLUGIN_TOKEN") || set("VIBEKE_HERDR_BROKER") {
        return Some(fail(
            "permission_denied",
            "plugin registry changes are not allowed from a pane or a plugin; ask the user",
            EXIT_PERMISSION,
        ));
    }
    None
}

/// Ask `question [y/N]` on the terminal; false without one.
fn confirm(question: &str) -> bool {
    if !std::io::stdin().is_terminal() {
        return false;
    }
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    matches!(line.trim(), "y" | "Y" | "yes")
}

/// Accepted capability names from `--accept a,b` / `--yes` (all shown), or by asking.
fn accepted(args: &[String], terms: &str) -> Option<Vec<String>> {
    if let Some(a) = value(args, "--accept") {
        return Some(a.split(',').map(|s| s.trim().to_string()).collect());
    }
    if flag(args, "--yes") || flag(args, "-y") {
        return Some(vec!["*".into()]);
    }
    eprintln!("{terms}");
    confirm("Grant these capabilities?").then(|| vec!["*".into()])
}

fn dirs() -> hreg::PluginDirs {
    vk_server::compat::plugin_dirs()
}

fn version() -> &'static str {
    vk_proto::VERSION
}

/// A source: an existing directory/manifest, or a fetched repository.
fn resolve(
    src: &str,
    git_ref: Option<&str>,
) -> Result<(PathBuf, Option<(source::Fetched, source::GitSource)>), i32> {
    let p = PathBuf::from(src);
    if p.exists() {
        let dir = if p.is_file() {
            p.parent().map(PathBuf::from).unwrap_or_default()
        } else {
            p
        };
        return Ok((dir, None));
    }
    match source::parse(src, git_ref) {
        Ok(Some(gs)) => match source::fetch(&gs, &source::base(), &dirs().checkouts.join(".fetch"))
        {
            Ok(f) => Ok((f.plugin_dir.clone(), Some((f, gs)))),
            Err(e) => Err(fail("fetch_failed", e, EXIT_API)),
        },
        Ok(None) => Err(fail(
            "invalid_params",
            format!("{src}: not found"),
            EXIT_USAGE,
        )),
        Err(e) => Err(fail("invalid_params", e, EXIT_USAGE)),
    }
}

fn cleanup(f: &Option<(source::Fetched, source::GitSource)>) {
    if let Some((f, _)) = f {
        let _ = std::fs::remove_dir_all(&f.work);
    }
}

fn is_native_id(id: &str) -> bool {
    Registry::load_shared(&dirs()).is_ok_and(|r| r.native.contains_key(id))
}

/// Entry point: `Some(exit code)` when the verb was a native one.
pub async fn cmd(g: &Global, args: &[String]) -> Option<i32> {
    let verb = args.first().map(String::as_str).unwrap_or("");
    let rest = args.get(1..).unwrap_or(&[]);
    let pos = positionals(rest);
    let first = pos.first().cloned();
    match verb {
        "install" => {
            let src = first?;
            let git_ref = value(rest, "--ref");
            let local = PathBuf::from(&src);
            // Local Herdr plugins fall through without fetching; repositories are fetched once
            // here and handed over only when they are not native.
            if local.exists() && !vk_compat::native::is_native(&local) {
                return None;
            }
            if !local.exists() && flag(rest, "--herdr") {
                return None;
            }
            Some(install(g, &src, git_ref.as_deref(), rest))
        }
        "link" => {
            let p = PathBuf::from(first?);
            if !vk_compat::native::is_native(&p) {
                return None;
            }
            Some(link(g, &p, rest))
        }
        "consent" => Some(match first {
            Some(id) => consent(g, &id, rest),
            None => fail("invalid_params", "vibeke plugin consent <id>", EXIT_USAGE),
        }),
        "update" if flag(rest, "--auto") => Some(auto_update(g)),
        "update" => {
            let id = first?;
            if !is_native_id(&id) {
                return None;
            }
            Some(update(g, &id, rest))
        }
        "uninstall" | "remove" | "unlink" | "enable" | "disable" | "untrust" | "revoke" => {
            let id = first?;
            if !is_native_id(&id) {
                return None;
            }
            Some(simple(g, verb, &id, rest))
        }
        "list" | "ls" if Registry::load(&dirs()).is_ok_and(|r| !r.native.is_empty()) => {
            Some(list(g))
        }
        "search" => Some(search(g, &pos.join(" "), value(rest, "--index"))),
        "export" => Some(match first {
            Some(d) => export(g, &d),
            None => fail("invalid_params", "vibeke plugin export <dir>", EXIT_USAGE),
        }),
        "import" => Some(match first {
            Some(d) => import(g, &d, flag(rest, "--dry-run")),
            None => fail("invalid_params", "vibeke plugin import <dir>", EXIT_USAGE),
        }),
        "restart" => Some(match first {
            Some(id) => api(g, "plugin.restart", json!({"plugin": id})).await,
            None => fail("invalid_params", "vibeke plugin restart <id>", EXIT_USAGE),
        }),
        "logs" | "log" if flag(rest, "-f") || flag(rest, "--follow") => {
            Some(follow(g, first.as_deref()).await)
        }
        _ => None,
    }
}

fn install(g: &Global, src: &str, git_ref: Option<&str>, args: &[String]) -> i32 {
    if let Some(c) = refused() {
        return c;
    }
    let (dir, fetched) = match resolve(src, git_ref) {
        Ok(x) => x,
        Err(c) => return c,
    };
    if !vk_compat::native::is_native(&dir) {
        // A repository with a Herdr manifest: the Herdr path fetches it again.
        cleanup(&fetched);
        return crate::compat::install_herdr_fallback(g, src, git_ref, args);
    }
    let d = dirs();
    let origin = match &fetched {
        Some((f, gs)) => nreg::git_origin(f, gs),
        None => nreg::local_origin(&dir, git_ref),
    };
    let staged = match nreg::stage(&d, &dir, origin) {
        Ok(s) => s,
        Err(e) => {
            cleanup(&fetched);
            return reg_fail(e);
        }
    };
    cleanup(&fetched);
    let m = staged.manifest.clone();
    if let Err(why) = m.compatible(version(), vk_compat::herdr::current_platform()) {
        staged.discard();
        return fail("unsupported", why, EXIT_API);
    }
    let prev = Registry::load_shared(&d)
        .ok()
        .and_then(|r| r.native.get(&m.id).and_then(|e| e.consent.clone()));
    let preview = nreg::NativeEntry {
        id: m.id.clone(),
        root: staged.dir.clone(),
        managed: true,
        origin: staged.origin.clone(),
        enabled: true,
        built: false,
        installed_at_ms: 0,
        tree_sha256: String::new(),
        consent: None,
    };
    let terms = consent_terms(&preview, &m, prev.as_ref().map(|c| &c.capabilities));
    if flag(args, "--dry-run") {
        staged.discard();
        print(
            g,
            &json!({"plugin": m.id, "dry_run": true, "requested_capabilities": m.capabilities.items(), "entrypoints": m.entrypoints(vk_compat::herdr::current_platform())}),
            || terms.clone(),
        );
        return EXIT_OK;
    }
    let Some(acc) = accepted(args, &terms) else {
        staged.discard();
        return fail(
            "permission_denied",
            "capabilities_not_accepted: nothing installed (pass --yes or --accept after reviewing)",
            EXIT_PERMISSION,
        );
    };
    let missing = m.capabilities.not_accepted(&acc);
    if !missing.is_empty() {
        staged.discard();
        return fail(
            "permission_denied",
            format!("capabilities_not_accepted: {}", missing.join(", ")),
            EXIT_PERMISSION,
        );
    }
    let d2 = d.clone();
    let (e, _) = match Registry::update(&d, move |r| r.native_install_staged(&d2, staged)) {
        Ok(x) => x,
        Err(e) => return reg_fail(e),
    };
    match consent_and_build(&d, &e.id, Some(&acc)) {
        Ok(c) => {
            print(
                g,
                &json!({"plugin": e.id, "kind": "native", "consent_id": c.consent_id, "root": e.root}),
                || format!("installed {} {} (native)", e.id, m.version),
            );
            EXIT_OK
        }
        Err(err) => reg_fail(err),
    }
}

fn link(g: &Global, p: &std::path::Path, args: &[String]) -> i32 {
    if let Some(c) = refused() {
        return c;
    }
    let d = dirs();
    let (e, m) = match Registry::update(&d, |r| r.native_link(p)) {
        Ok(x) => x,
        Err(e) => return reg_fail(e),
    };
    let terms = consent_terms(&e, &m, e.consent.as_ref().map(|c| &c.capabilities));
    let needs = native_status(&e, version()).0;
    if matches!(
        needs,
        NativeStatus::NeedsConsent | NativeStatus::Reconsent(_)
    ) {
        match accepted(args, &terms) {
            Some(acc) => {
                if let Err(err) = consent_and_build(&d, &e.id, Some(&acc)) {
                    return reg_fail(err);
                }
            }
            None => {
                print(
                    g,
                    &json!({"plugin": e.id, "status": needs.as_str()}),
                    || {
                        format!(
                            "linked {} (inactive until `vibeke plugin consent {}`)",
                            e.id, e.id
                        )
                    },
                );
                return EXIT_OK;
            }
        }
    }
    print(
        g,
        &json!({"plugin": e.id, "dev": true, "root": e.root}),
        || format!("linked {} from {} (dev mode)", e.id, e.root.display()),
    );
    EXIT_OK
}

fn consent(g: &Global, id: &str, args: &[String]) -> i32 {
    if let Some(c) = refused() {
        return c;
    }
    let d = dirs();
    let e = match Registry::load_shared(&d).and_then(|r| r.native_get(id).cloned()) {
        Ok(e) => e,
        Err(e) => return reg_fail(e),
    };
    let m = match nreg::read_manifest(&e.root) {
        Ok((m, _)) => m,
        Err(e) => return reg_fail(e),
    };
    let terms = consent_terms(&e, &m, e.consent.as_ref().map(|c| &c.capabilities));
    let Some(acc) = accepted(args, &terms) else {
        return fail(
            "permission_denied",
            "capabilities_not_accepted",
            EXIT_PERMISSION,
        );
    };
    match consent_and_build(&d, id, Some(&acc)) {
        Ok(c) => {
            print(
                g,
                &json!({"plugin": id, "consent_id": c.consent_id}),
                || format!("{id}: capabilities approved"),
            );
            EXIT_OK
        }
        Err(e) => reg_fail(e),
    }
}

/// Re-fetch a repository install; returns the capability diff and whether it was applied.
fn refetch(id: &str) -> Result<(nreg::NativeEntry, nreg::NativeStaged), i32> {
    let d = dirs();
    let e = Registry::load_shared(&d)
        .and_then(|r| r.native_get(id).cloned())
        .map_err(reg_fail)?;
    let Some(repo) = e.origin.repo.clone() else {
        return Err(fail(
            "invalid_params",
            format!("{id} was not installed from a repository"),
            EXIT_API,
        ));
    };
    let (dir, fetched) = resolve(&repo, e.origin.requested_ref.as_deref())?;
    let origin = match &fetched {
        Some((f, gs)) => nreg::git_origin(f, gs),
        None => nreg::local_origin(&dir, None),
    };
    let staged = nreg::stage(&d, &dir, origin);
    cleanup(&fetched);
    Ok((e, staged.map_err(reg_fail)?))
}

fn update(g: &Global, id: &str, args: &[String]) -> i32 {
    if let Some(c) = refused() {
        return c;
    }
    let (e, staged) = match refetch(id) {
        Ok(x) => x,
        Err(c) => return c,
    };
    if staged.origin.commit == e.origin.commit && staged.tree_sha256 == e.tree_sha256 {
        staged.discard();
        print(g, &json!({"plugin": id, "updated": false}), || {
            format!("{id} is up to date")
        });
        return EXIT_OK;
    }
    let old = e
        .consent
        .as_ref()
        .map(|c| c.capabilities.clone())
        .unwrap_or_default();
    let df = nreg::diff(&old, &staged.manifest.capabilities);
    let d = dirs();
    let d2 = d.clone();
    if let Err(err) = Registry::update(&d, move |r| {
        r.native_install_staged(&d2, staged).map(|_| ())
    }) {
        return reg_fail(err);
    }
    let mut status = "active";
    if df.widens {
        if flag(args, "--yes") {
            if let Err(err) = consent_and_build(&d, id, Some(&["*".to_string()])) {
                return reg_fail(err);
            }
        } else {
            status = "reconsent";
        }
    } else {
        // Same consent; a new checkout may need its build.
        let consent_id = Registry::load_shared(&d)
            .ok()
            .and_then(|r| r.native.get(id).and_then(|e| e.consent.clone()))
            .map(|c| c.consent_id);
        if consent_id.is_some()
            && let Err(err) = rebuild(&d, id)
        {
            return reg_fail(err);
        }
    }
    print(
        g,
        &json!({"plugin": id, "updated": true, "added": df.added, "removed": df.removed, "status": status}),
        || {
            let mut s = format!("updated {id}");
            if !df.added.is_empty() {
                s.push_str(&format!("\n  new capabilities: {}", df.added.join(", ")));
            }
            if !df.removed.is_empty() {
                s.push_str(&format!("\n  dropped: {}", df.removed.join(", ")));
            }
            if status == "reconsent" {
                s.push_str(&format!("\n  inactive until `vibeke plugin consent {id}`"));
            }
            s
        },
    );
    EXIT_OK
}

/// Run the build of an updated managed checkout under the existing consent.
fn rebuild(d: &hreg::PluginDirs, id: &str) -> Result<(), RegistryError> {
    let reg = Registry::load_shared(d)?;
    let e = reg.native_get(id)?.clone();
    let (m, digest) = nreg::read_manifest(&e.root)?;
    if e.built || m.build_on(vk_compat::herdr::current_platform()).is_empty() {
        return Ok(());
    }
    let consent_id = e
        .consent
        .as_ref()
        .map(|c| c.consent_id.clone())
        .unwrap_or_default();
    nreg::run_build(&e, &m, &digest).map_err(RegistryError::Conflict)?;
    Registry::update(d, |r| r.native_finish_build(id, &consent_id))
}

/// `update --auto`: same major.minor and non-widening updates of repository installs only.
fn auto_update(g: &Global) -> i32 {
    let d = dirs();
    let reg = match Registry::load_shared(&d) {
        Ok(r) => r,
        Err(e) => return reg_fail(e),
    };
    let mut applied = vec![];
    let mut skipped = vec![];
    for e in reg.native.values().filter(|e| e.origin.repo.is_some()) {
        let Ok((cur, staged)) = refetch(&e.id) else {
            skipped.push(json!({"plugin": e.id, "reason": "fetch failed"}));
            continue;
        };
        if staged.origin.commit == cur.origin.commit {
            staged.discard();
            continue;
        }
        let old_v = cur
            .consent
            .as_ref()
            .map(|c| c.version.clone())
            .unwrap_or_default();
        let mm = |v: &str| vk_compat::herdr::parse_version(v).map(|(a, b, _)| (a, b));
        let patch = mm(&old_v).is_some() && mm(&old_v) == mm(&staged.manifest.version);
        let widens = cur.consent.as_ref().is_none_or(|c| {
            !staged
                .manifest
                .capabilities
                .widened_from(&c.capabilities)
                .is_empty()
        });
        if !patch || widens {
            skipped.push(json!({"plugin": e.id, "reason": if widens { "widens capabilities" } else { "not a patch update" }}));
            staged.discard();
            continue;
        }
        let d2 = d.clone();
        if Registry::update(&d, move |r| {
            r.native_install_staged(&d2, staged).map(|_| ())
        })
        .is_ok()
            && rebuild(&d, &e.id).is_ok()
        {
            applied.push(e.id.clone());
        }
    }
    print(g, &json!({"applied": applied, "skipped": skipped}), || {
        format!(
            "auto-update: {} applied, {} skipped",
            applied.len(),
            skipped.len()
        )
    });
    EXIT_OK
}

fn simple(g: &Global, verb: &str, id: &str, _args: &[String]) -> i32 {
    if let Some(c) = refused() {
        return c;
    }
    let d = dirs();
    let r = match verb {
        "enable" => Registry::update(&d, |r| r.native_set_enabled(id, true).map(|_| ())),
        "disable" => Registry::update(&d, |r| r.native_set_enabled(id, false).map(|_| ())),
        "untrust" | "revoke" => Registry::update(&d, |r| r.native_revoke(id).map(|_| ())),
        _ => {
            let d2 = d.clone();
            Registry::update(&d, move |r| r.native_remove(&d2, id).map(|_| ()))
        }
    };
    match r {
        Ok(()) => {
            print(g, &json!({"plugin": id, "done": verb}), || {
                format!("{id}: {verb} done")
            });
            EXIT_OK
        }
        Err(e) => reg_fail(e),
    }
}

fn list(g: &Global) -> i32 {
    let d = dirs();
    let reg = match Registry::load(&d) {
        Ok(r) => r,
        Err(e) => return reg_fail(e),
    };
    let mut rows: Vec<Value> = reg
        .plugins
        .values()
        .map(|e| {
            let (st, m) = hreg::entry_status(e);
            json!({"plugin_id": e.id, "kind": "herdr", "status": st.as_str(), "enabled": e.enabled,
                   "version": m.and_then(|m| m.version), "root": e.root})
        })
        .collect();
    rows.extend(reg.native.values().map(|e| {
        let (st, m) = native_status(e, version());
        json!({"plugin_id": e.id, "kind": "native", "status": st.as_str(), "status_detail": st.detail(),
               "enabled": e.enabled, "version": m.map(|m| m.version), "root": e.root, "dev": !e.managed})
    }));
    print(g, &json!({"plugins": rows}), || {
        if rows.is_empty() {
            return "no plugins".into();
        }
        rows.iter()
            .map(|p| {
                format!(
                    "{:<36} {:<7} {:<14} {}",
                    p["plugin_id"].as_str().unwrap_or(""),
                    p["kind"].as_str().unwrap_or(""),
                    p["status"].as_str().unwrap_or(""),
                    p["root"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    EXIT_OK
}

fn search(g: &Global, q: &str, url: Option<String>) -> i32 {
    let url = url
        .or_else(|| {
            vk_server::plugin_native::setting("", "index_url")
                .and_then(|v| v.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| index::DEFAULT_URL.to_string());
    let idx = match index::fetch(&url) {
        Ok(i) => i,
        Err(e) => {
            return fail(
                "unavailable",
                format!("{e} (the hosted index is post-1.0; set [plugins] index_url)"),
                EXIT_API,
            );
        }
    };
    let hits: Vec<&index::IndexEntry> = idx.search(q);
    print(g, &json!({"results": hits}), || {
        if hits.is_empty() {
            return format!("no plugins match `{q}`");
        }
        hits.iter()
            .map(|h| {
                format!(
                    "{:<32} {:<8} ★{:<5} {}  {}",
                    h.id,
                    h.kind,
                    h.stars,
                    h.repo,
                    h.description.clone().unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    EXIT_OK
}

fn export(g: &Global, dir: &str) -> i32 {
    match backup::export(&dirs(), std::path::Path::new(dir)) {
        Ok(r) => {
            print(g, &json!(r), || {
                format!("exported {} plugins to {dir}", r.plugins.len())
            });
            EXIT_OK
        }
        Err(e) => reg_fail(e),
    }
}

fn import(g: &Global, dir: &str, dry: bool) -> i32 {
    if let Some(c) = refused() {
        return c;
    }
    match backup::import(&dirs(), std::path::Path::new(dir), dry) {
        Ok(r) => {
            print(g, &json!(r), || {
                let mut s = format!(
                    "{} {} plugins, {} files",
                    if dry { "would import" } else { "imported" },
                    r.plugins.len(),
                    r.files
                );
                for c in &r.conflicts {
                    s.push_str(&format!("\n  conflict: {c}"));
                }
                for c in &r.needs_review {
                    s.push_str(&format!(
                        "\n  review again: vibeke plugin trust {c} --legacy"
                    ));
                }
                s
            });
            EXIT_OK
        }
        Err(e) => reg_fail(e),
    }
}

async fn api(g: &Global, method: &str, params: Value) -> i32 {
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    let stream = match client::connect_or_spawn(&g.session, &socket, g.no_spawn).await {
        Ok(s) => s,
        Err(e) => return fail("server_unavailable", format!("{e:#}"), EXIT_NO_SERVER),
    };
    let mut c = Client::new(stream);
    crate::run_api(&mut c, g, method, params).await
}

/// `plugin logs <id> -f`: print new records as they appear (every second).
async fn follow(g: &Global, plugin: Option<&str>) -> i32 {
    let socket = client::socket_path(&g.session, g.socket.as_deref());
    let stream = match client::connect_or_spawn(&g.session, &socket, true).await {
        Ok(s) => s,
        Err(e) => return fail("server_unavailable", format!("{e:#}"), EXIT_NO_SERVER),
    };
    let mut c = Client::new(stream);
    if c.hello("cli").await.is_err() {
        return fail("server_unavailable", "hello failed", EXIT_NO_SERVER);
    }
    let mut seen: std::collections::HashMap<String, String> = Default::default();
    loop {
        let mut p = json!({"limit": 200});
        if let Some(pl) = plugin {
            p["plugin"] = json!(pl);
        }
        let Ok(v) = c.call("plugin.log.list", p).await else {
            return fail("server_unavailable", "connection lost", EXIT_NO_SERVER);
        };
        for r in v["logs"].as_array().cloned().unwrap_or_default() {
            let id = r["id"].as_str().unwrap_or_default().to_string();
            let st = r["status"].as_str().unwrap_or_default().to_string();
            if seen.get(&id) == Some(&st) {
                continue;
            }
            seen.insert(id.clone(), st.clone());
            if json_out(g) {
                println!("{r}");
            } else {
                let what = r["action_id"]
                    .as_str()
                    .or(r["event"].as_str())
                    .unwrap_or("");
                println!(
                    "{} {:<10} {} {}",
                    r["plugin_id"].as_str().unwrap_or(""),
                    st,
                    what,
                    id
                );
                for k in ["stdout_tail", "stderr_tail"] {
                    if let Some(t) = r[k].as_str().filter(|t| !t.is_empty() && st != "running") {
                        for l in t.lines() {
                            println!("  {l}");
                        }
                    }
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}
