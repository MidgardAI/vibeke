//! `config.*` and `server.reload_config` (07 §2.14, 08 §11.2): read the effective config, set keys
//! (a runtime override, or persisted to `config.toml` with comments kept and an atomic write),
//! validate a file, and reload, from the API or from the config file watcher. Every applied
//! change emits `session.config_reloaded {changed_keys}`; a file that fails to parse or validate
//! never replaces the applied config (`session.config_rejected`). `config.get` reports each
//! key's layer (`default < user < repo < runtime < cli`, `vk_config::layers`); with `repo`,
//! `cwd` or `pane` it layers that repository's trusted `.vibeke/config.toml` in.

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, req, s};
use crate::core::Tx;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use vk_config::{Config, ConfigError};
use vk_proto::rpc::ErrorKind;

pub const METHODS: &[(&str, bool)] = &[
    ("config.get", false),
    ("config.set", true),
    ("config.validate", false),
    ("config.reload", true),
];

/// The config last applied by this server (what `Config::diff` compares against).
fn applied() -> &'static Mutex<Option<Config>> {
    static A: OnceLock<Mutex<Option<Config>>> = OnceLock::new();
    A.get_or_init(Mutex::default)
}

/// Serializes `config.set` read-modify-write cycles.
fn set_lock() -> &'static tokio::sync::Mutex<()> {
    static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    L.get_or_init(tokio::sync::Mutex::default)
}

/// The config this server applies now (loaded on first use).
pub fn current() -> Config {
    let mut a = applied().lock().unwrap();
    if a.is_none() {
        *a = Some(load_effective().unwrap_or_default());
    }
    a.clone().unwrap_or_default()
}

fn load_effective() -> Result<Config, ConfigError> {
    Config::load(vk_config::config_path()).map(|(c, _)| c)
}

pub fn diagnostics(e: &ConfigError) -> Vec<Value> {
    match e {
        ConfigError::Parse(d) => vec![json!({"line": d.line, "col": d.col, "message": d.message})],
        ConfigError::Invalid(v) => v
            .iter()
            .map(|d| json!({"line": d.line, "col": d.col, "message": d.message}))
            .collect(),
        ConfigError::Io { .. } => vec![json!({"line": 0, "col": 0, "message": e.to_string()})],
    }
}

fn warnings_json(w: &[vk_config::Warning]) -> Vec<Value> {
    w.iter()
        .map(|w| {
            json!({"key": w.key, "line": w.pos.map(|p| p.line), "col": w.pos.map(|p| p.col), "message": w.message})
        })
        .collect()
}

/// Start the file watcher (`config.watch`, default on). Runs for the server's life.
pub fn start(server: &Arc<Server>) {
    let cfg = current();
    crate::limits::refresh(&cfg);
    if !cfg.config.watch {
        return;
    }
    let path = vk_config::config_path();
    let (watcher, rx) = match vk_config::watch(&path, cfg, Duration::from_millis(250)) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "config watcher not started");
            return;
        }
    };
    let srv = server.clone();
    std::thread::Builder::new()
        .name("config-watch".into())
        .spawn(move || {
            let _keep = watcher;
            while let Ok(ev) = rx.recv() {
                match ev {
                    vk_config::ReloadEvent::Reloaded { config, .. } => {
                        apply(&srv, *config, "watch");
                    }
                    vk_config::ReloadEvent::Rejected(e) => {
                        tracing::warn!(error = %e, "config change rejected; keeping the applied config");
                        let mut c = srv.core.lock().unwrap();
                        let mut tx = Tx::new();
                        tx.event(
                            "session.config_rejected",
                            json!({}),
                            json!({"errors": diagnostics(&e), "source": "watch"}),
                        );
                        let _ = srv.commit(&mut c, tx);
                    }
                }
            }
        })
        .ok();
}

/// Apply a newly loaded config: diff against the applied one, store it, emit the event.
/// Returns the changed keys.
fn apply(server: &Server, new: Config, source: &str) -> Vec<String> {
    let changed = {
        let mut a = applied().lock().unwrap();
        let old = a.clone().unwrap_or_default();
        let changed = Config::diff(&old, &new);
        *a = Some(new.clone());
        changed
    };
    crate::limits::refresh(&new);
    if !changed.is_empty() {
        let new_panes_only: Vec<&String> = changed
            .iter()
            .filter(|k| vk_config::requires_new_panes(k))
            .collect();
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event(
            "session.config_reloaded",
            json!({}),
            json!({"changed_keys": changed, "new_panes_only": new_panes_only, "source": source}),
        );
        let _ = server.commit(&mut c, tx);
    }
    changed
}

/// Reload from disk (runtime overrides applied on top). Errors leave the applied config alone.
pub fn reload(server: &Server, source: &str) -> R {
    match Config::load(vk_config::config_path()) {
        Ok((cfg, warnings)) => {
            let changed = apply(server, cfg, source);
            Ok(json!({"changed": changed, "errors": [], "warnings": warnings_json(&warnings)}))
        }
        Err(e) => Ok(json!({"changed": [], "errors": diagnostics(&e)})),
    }
}

fn to_json(v: &toml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn from_json(v: &Value) -> Result<Option<toml::Value>, String> {
    Ok(Some(match v {
        Value::Null => return Ok(None),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => toml::Value::Integer(i),
            None => toml::Value::Float(n.as_f64().ok_or("number out of range")?),
        },
        Value::String(s) => toml::Value::String(s.clone()),
        Value::Array(a) => toml::Value::Array(
            a.iter()
                .map(|x| from_json(x)?.ok_or_else(|| "null inside an array".to_string()))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(o) => {
            let mut t = toml::Table::new();
            for (k, x) in o {
                if let Some(x) = from_json(x)? {
                    t.insert(k.clone(), x);
                }
            }
            toml::Value::Table(t)
        }
    }))
}

/// Where a key's effective value comes from, highest layer first (08 §11): `cli`, `runtime`,
/// `repo` (a trusted repository's `.vibeke/config.toml`), `user`, `default`.
fn source_of(
    key: &str,
    file: Option<&toml::Value>,
    repo: Option<&vk_config::RepoConfig>,
) -> &'static str {
    let covered = |k: &str| {
        key == k || key.starts_with(&format!("{k}.")) || k.starts_with(&format!("{key}."))
    };
    if vk_config::layers::cli_overrides()
        .keys()
        .any(|k| covered(k))
    {
        return "cli";
    }
    if vk_config::edit::runtime_overrides()
        .keys()
        .any(|k| covered(k))
    {
        return "runtime";
    }
    if let Some(r) = repo {
        let policy = !r.policy_rules.is_empty() && covered("policy.rule");
        let commands = !r.commands.is_empty() && covered("keys.command");
        if policy
            || commands
            || vk_config::layers::repo_keys(r)
                .iter()
                .any(|(k, _)| covered(k))
        {
            return "repo";
        }
    }
    if file.is_some_and(|f| vk_config::edit::lookup(f, key).is_some()) {
        return "user";
    }
    "default"
}

/// The repo layer for `config.get {repo | cwd | pane}`: the repository's `.vibeke/config.toml`,
/// applied only while its `.vibeke/` tree is trusted (09 §4).
fn repo_layer(server: &Server, p: &Value) -> (Option<vk_config::RepoConfig>, Value) {
    let dir = s(p, "repo")
        .or_else(|| s(p, "cwd"))
        .map(std::path::PathBuf::from)
        .or_else(|| {
            let t = s(p, "pane")?;
            let id = server.with_core(|c| {
                c.pane(t)
                    .or_else(|| c.model.panes.iter().find(|x| x.handle == t))
                    .map(|x| x.id.clone())
            })?;
            server.pane_cwd(&id).map(std::path::PathBuf::from)
        });
    let Some(dir) = dir else {
        return (None, Value::Null);
    };
    let root = vk_tasks::repo_root(&dir)
        .map(|i| i.root)
        .or_else(|| vk_config::repo::find(&dir));
    let Some(root) = root else {
        return (None, Value::Null);
    };
    let root = root.canonicalize().unwrap_or(root);
    let file = root.join(vk_config::REPO_CONFIG);
    let (_, trusted) = crate::repo_config::trusted(server, &root);
    match vk_config::repo::load(&root) {
        None => (
            None,
            json!({"root": root, "file": Value::Null, "trusted": trusted, "applied": false}),
        ),
        Some(Err(e)) => (
            None,
            json!({"root": root, "file": file, "trusted": trusted, "applied": false, "error": e}),
        ),
        Some(Ok(rc)) => {
            let info = json!({"root": root, "file": file, "trusted": trusted, "applied": trusted, "warnings": rc.warnings});
            (trusted.then_some(rc), info)
        }
    }
}

fn file_text() -> Result<Option<String>, String> {
    let path = vk_config::config_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn config_get(server: &Server, p: &Value) -> R {
    let path = vk_config::config_path();
    let (repo, repo_info) = repo_layer(server, p);
    let (cfg, errors) = match Config::load_layered(&path, repo.as_ref()) {
        Ok((c, _)) => (c, vec![]),
        Err(e) => (current(), diagnostics(&e)),
    };
    let root = cfg.to_value();
    let file: Option<toml::Value> = file_text()
        .ok()
        .flatten()
        .and_then(|t| toml::from_str(&t).ok());
    let overrides: Vec<String> = vk_config::edit::runtime_overrides().into_keys().collect();
    let cli: Vec<String> = vk_config::layers::cli_overrides().into_keys().collect();
    let mut layers = vec![
        json!({"source": "default"}),
        json!({"source": "user", "path": path, "exists": file.is_some()}),
    ];
    if !repo_info.is_null() {
        layers.push(json!({"source": "repo", "path": repo_info["file"], "trusted": repo_info["trusted"], "applied": repo_info["applied"]}));
    }
    layers.push(json!({"source": "runtime", "keys": overrides}));
    layers.push(json!({"source": "cli", "keys": cli}));
    match s(p, "key").filter(|k| !k.is_empty()) {
        Some(key) => {
            vk_config::edit::split_key(key).map_err(invalid)?;
            let Some(v) = vk_config::edit::lookup(&root, key) else {
                return Err(err(ErrorKind::NotFound, format!("no config key `{key}`"))
                    .details(json!({"object": "config_key", "target": key})));
            };
            Ok(
                json!({"key": key, "value": to_json(v), "source": source_of(key, file.as_ref(), repo.as_ref()), "path": path, "overrides": overrides, "cli_overrides": cli, "layers": layers, "repo": repo_info, "errors": errors}),
            )
        }
        None => Ok(
            json!({"value": to_json(&root), "source": if file.is_some() { "user" } else { "default" }, "path": path, "overrides": overrides, "cli_overrides": cli, "layers": layers, "repo": repo_info, "errors": errors}),
        ),
    }
}

async fn config_set(server: &Server, p: &Value) -> R {
    let key = req(p, "key")?.to_string();
    vk_config::edit::split_key(&key).map_err(invalid)?;
    let value = from_json(p.get("value").unwrap_or(&Value::Null)).map_err(invalid)?;
    let persist = b(p, "persist").unwrap_or(false);
    let _guard = set_lock().lock().await;
    let path = vk_config::config_path();
    let file = file_text().map_err(internal)?.unwrap_or_default();
    // The candidate the server would apply: file + other runtime overrides + this key.
    let edited_file = vk_config::edit::edit_text(&file, &key, value.as_ref()).map_err(invalid)?;
    let mut candidate: toml_edit::DocumentMut = edited_file
        .parse()
        .map_err(|e| invalid(format!("config.toml does not parse: {e}")))?;
    for (k, v) in vk_config::edit::runtime_overrides() {
        if k != key {
            let _ = vk_config::edit::set_in_doc(&mut candidate, &k, v.as_ref());
        }
    }
    let (_, warnings) = Config::parse(&candidate.to_string(), &path).map_err(|e| {
        invalid(format!("`{key}`: {e}")).details(json!({"errors": diagnostics(&e)}))
    })?;
    // An unknown key would be stored and silently ignored: refuse it.
    if let Some(w) = warnings
        .iter()
        .find(|w| w.key == key || key.starts_with(&format!("{}.", w.key)))
    {
        return Err(invalid(format!("`{key}`: {}", w.message))
            .details(json!({"warnings": warnings_json(std::slice::from_ref(w))})));
    }
    if persist {
        vk_config::edit::write_atomic(&path, &edited_file).map_err(internal)?;
        vk_config::edit::clear_runtime_override(&key);
    } else {
        vk_config::edit::set_runtime_override(&key, value.clone());
    }
    let r = reload(server, if persist { "set_persist" } else { "set" })?;
    if r["errors"].as_array().is_some_and(|e| !e.is_empty()) {
        return Err(internal(format!("config reload failed: {}", r["errors"])));
    }
    Ok(json!({
        "key": key,
        "value": value.as_ref().map(to_json).unwrap_or(Value::Null),
        "persisted": persist,
        "path": path,
        "changed": r["changed"],
    }))
}

fn config_validate(ctx: &Ctx, p: &Value) -> R {
    // From a pane, `path` would be a file existence and parse oracle for any file.
    if ctx.pane_scope.is_some() && p.get("path").is_some_and(|v| !v.is_null()) {
        return Err(err(
            ErrorKind::PermissionDenied,
            "config.validate {path} needs a user client; a pane validates the user config only",
        ));
    }
    let path = s(p, "path")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(vk_config::config_path);
    let res = match std::fs::read_to_string(&path) {
        Ok(src) => Config::parse(&src, &path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(crate::api::not_found("file", &path.display().to_string()));
        }
        Err(e) => Err(ConfigError::Io {
            path: path.clone(),
            source: e,
        }),
    };
    Ok(match res {
        Ok((_, w)) => {
            json!({"path": path, "valid": true, "errors": [], "warnings": warnings_json(&w)})
        }
        Err(e) => json!({"path": path, "valid": false, "errors": diagnostics(&e), "warnings": []}),
    })
}

/// Dispatch hook for `config.*` and `server.reload_config`.
pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "config.get" => config_get(server, p),
        "config.set" => config_set(server, p).await,
        "config.validate" => config_validate(ctx, p),
        "config.reload" | "server.reload_config" => reload(server, "api"),
        _ => return None,
    })
}
