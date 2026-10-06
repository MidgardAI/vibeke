//! Task `[previews]` (06 B2, 05 §6): previews declared by task config, with ports taken from
//! the task's port lease so the port is known before the dev server starts.
//!
//! ```toml
//! [ports]
//! env = { PORT = 0, API_PORT = 1 }        # offsets into the leased block
//!
//! [previews]
//! web = { port_env = "PORT", path = "/" }
//! api = { port_env = "API_PORT", label = "api", scheme = "https" }
//! docs = { offset = 5 }
//! storybook = "STORYBOOK_PORT"             # shorthand for { port_env = … }
//! ```
//!
//! Callers parse TOML into JSON values; this module stays format-agnostic. Repo-local config
//! may only name ports **inside the task's lease** (`port_env` / `offset`); an absolute
//! `port = N` is honoured only when the caller allows it (user-supplied `task.create` params),
//! so a repository can't make previews (and with them headless-browser reachability, 06 B5)
//! point at arbitrary local services.

use crate::ports::Lease;
use serde::Serialize;
use serde_json::Value;

/// At most this many task previews.
pub const MAX_TASK_PREVIEWS: usize = 16;

/// One `[previews]` entry as written.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreviewSpec {
    pub name: String,
    pub port_env: Option<String>,
    pub offset: Option<u16>,
    pub port: Option<u16>,
    pub path: Option<String>,
    pub label: Option<String>,
    pub scheme: Option<String>,
    /// Serve this preview over https in proxy mode (06 B4 `tls_origin`); `None` = the
    /// `[previews]` default, then the global `[preview] tls_origin`.
    pub tls_origin: Option<bool>,
}

/// A task preview with its port resolved from the lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedPreview {
    pub name: String,
    pub port: u16,
    pub path: String,
    pub label: String,
    pub scheme: String,
    /// Per-preview `tls_origin` (entry, else the `[previews]` default); `None` = global setting.
    pub tls_origin: Option<bool>,
    /// How the port was chosen (`PORT`, `offset 5`, `port 3000`).
    pub from: String,
}

fn u16_of(v: &Value) -> Option<u16> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .and_then(|n| u16::try_from(n).ok())
}

/// Parse a `[previews]` table. Unknown keys are ignored; malformed entries become warnings.
pub fn parse_previews(table: &Value) -> (Vec<PreviewSpec>, Vec<String>) {
    let mut out = Vec::new();
    let mut warn = Vec::new();
    let Some(t) = table.as_object() else {
        if !table.is_null() {
            warn.push("[previews] must be a table".to_string());
        }
        return (out, warn);
    };
    for (name, v) in t {
        // `[previews] tls_origin = true`: the table-wide default, not an entry.
        if name == "tls_origin" && v.is_boolean() {
            continue;
        }
        let mut s = PreviewSpec {
            name: name.clone(),
            ..Default::default()
        };
        match v {
            Value::String(env) => s.port_env = Some(env.clone()),
            Value::Object(o) => {
                let str_of = |k: &str| o.get(k).and_then(Value::as_str).map(str::to_string);
                s.port_env = str_of("port_env");
                s.offset = o.get("offset").and_then(u16_of);
                s.port = o.get("port").and_then(u16_of);
                s.path = str_of("path");
                s.label = str_of("label");
                s.scheme = str_of("scheme");
                s.tls_origin = o.get("tls_origin").and_then(Value::as_bool);
            }
            _ => {
                warn.push(format!(
                    "preview {name}: expected a table or a port_env name"
                ));
                continue;
            }
        }
        if s.port_env.is_none() && s.offset.is_none() && s.port.is_none() {
            warn.push(format!("preview {name}: needs port_env, offset or port"));
            continue;
        }
        out.push(s);
    }
    (out, warn)
}

/// The table-wide `[previews] tls_origin = true|false` default, if set.
pub fn previews_tls_default(table: &Value) -> Option<bool> {
    table.get("tls_origin").and_then(Value::as_bool)
}

/// `[ports] env = { NAME = offset }` → (name, offset). `PORT` defaults to offset 0.
pub fn port_env_offsets(ports: Option<&Value>) -> Vec<(String, u16)> {
    let mut v: Vec<(String, u16)> = ports
        .and_then(|p| p.get("env"))
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| u16_of(v).map(|n| (k.clone(), n)))
                .collect()
        })
        .unwrap_or_default();
    if !v.iter().any(|(k, _)| k == "PORT") {
        v.push(("PORT".into(), 0));
    }
    v
}

/// Offset of an env name in the lease: the `[ports] env` table, `VIBEKE_PORT_BASE` (0) and
/// `VIBEKE_PORT_<i>` (the names [`crate::setup_env`] exports).
pub fn offset_of_env(name: &str, offsets: &[(String, u16)]) -> Option<u16> {
    if let Some((_, o)) = offsets.iter().find(|(k, _)| k == name) {
        return Some(*o);
    }
    if name == "VIBEKE_PORT_BASE" {
        return Some(0);
    }
    name.strip_prefix("VIBEKE_PORT_")
        .and_then(|i| i.parse().ok())
}

/// A preview path as an absolute-path reference on the preview's own origin: `/` followed by
/// anything but `/` or `\`. A missing leading `/` is added (`app` → `/app`). Refused: URLs
/// and other scheme-prefixed values (`https://x`, `javascript:…`), network-path references
/// (`//host/…`), any backslash (browsers treat `\` like `/`, so `/\host` is `//host`), and
/// whitespace or control characters. Query and fragment are kept (`/app?x=1#/route`).
pub fn normalize_preview_path(raw: &str) -> Result<String, &'static str> {
    if raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("path must not contain whitespace or control characters");
    }
    if raw.contains('\\') {
        return Err("path must not contain a backslash");
    }
    if !raw.starts_with('/') {
        // `scheme:` before any `/`, `?` or `#` is a URL (or `javascript:`), not a path.
        let head = raw.split(['/', '?', '#']).next().unwrap_or("");
        if let Some((scheme, _)) = head.split_once(':')
            && scheme
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            return Err("path must be a path on the preview (`/app`), not a URL");
        }
    }
    let path = if raw.starts_with('/') {
        raw.to_string()
    } else {
        format!("/{raw}")
    };
    if path.starts_with("//") {
        return Err("path must not start with `//` (that names another host)");
    }
    Ok(path)
}

/// Resolve specs against the lease. `allow_absolute` permits `port = N` (user-supplied config
/// only). Returns the previews and warnings for skipped entries.
pub fn resolve_previews(
    specs: &[PreviewSpec],
    lease: Option<&Lease>,
    offsets: &[(String, u16)],
    allow_absolute: bool,
) -> (Vec<ResolvedPreview>, Vec<String>) {
    let mut out: Vec<ResolvedPreview> = Vec::new();
    let mut warn = Vec::new();
    for s in specs {
        if out.len() >= MAX_TASK_PREVIEWS {
            warn.push(format!(
                "more than {MAX_TASK_PREVIEWS} previews; {} and later skipped",
                s.name
            ));
            break;
        }
        let in_lease = |offset: u16, from: String| -> Result<(u16, String), String> {
            let l =
                lease.ok_or_else(|| format!("preview {}: the task has no port lease", s.name))?;
            l.port(offset).map(|p| (p, from)).ok_or_else(|| {
                format!(
                    "preview {}: offset {offset} is outside the leased block {}-{}",
                    s.name, l.start, l.end
                )
            })
        };
        let port = if let Some(env) = &s.port_env {
            match offset_of_env(env, offsets) {
                Some(o) => in_lease(o, env.clone()),
                None => Err(format!(
                    "preview {}: {env} is not a leased port (add it to [ports] env)",
                    s.name
                )),
            }
        } else if let Some(o) = s.offset {
            in_lease(o, format!("offset {o}"))
        } else if let Some(p) = s.port.filter(|p| *p != 0) {
            if allow_absolute {
                Ok((p, format!("port {p}")))
            } else {
                Err(format!(
                    "preview {}: repo config may only use leased ports (port_env/offset), not port = {p}",
                    s.name
                ))
            }
        } else {
            Err(format!("preview {}: no port", s.name))
        };
        let (port, from) = match port {
            Ok(x) => x,
            Err(e) => {
                warn.push(e);
                continue;
            }
        };
        let path = match normalize_preview_path(s.path.as_deref().unwrap_or("/")) {
            Ok(p) => p,
            Err(e) => {
                warn.push(format!("preview {}: {e}", s.name));
                continue;
            }
        };
        let scheme = s.scheme.clone().unwrap_or_else(|| "http".into());
        if scheme != "http" && scheme != "https" {
            warn.push(format!("preview {}: scheme must be http or https", s.name));
            continue;
        }
        if let Some(other) = out.iter().find(|x| x.port == port) {
            warn.push(format!(
                "preview {}: port {port} is already used by preview {}",
                s.name, other.name
            ));
            continue;
        }
        out.push(ResolvedPreview {
            name: s.name.clone(),
            port,
            path,
            label: s.label.clone().unwrap_or_else(|| s.name.clone()),
            scheme,
            tls_origin: s.tls_origin,
            from,
        });
    }
    (out, warn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preview_paths_are_absolute_path_references() {
        for (raw, want) in [
            ("/", "/"),
            ("app", "/app"),
            ("/app?x=1", "/app?x=1"),
            ("/#/dashboard", "/#/dashboard"),
            ("/a/b#frag", "/a/b#frag"),
            ("/app:1/x", "/app:1/x"),
            ("/a//b", "/a//b"),
        ] {
            assert_eq!(normalize_preview_path(raw).as_deref(), Ok(want), "{raw}");
        }
        for raw in [
            "//attacker.example/",
            "//attacker.example",
            "/\\attacker.example/",
            "\\attacker.example",
            "\\\\attacker.example/",
            "/app\\x",
            "https://attacker.example/",
            "http:attacker.example",
            "javascript:alert(1)",
            "localhost:3000/x",
            "/a b",
            "/a\tb",
            "/a\nb",
        ] {
            assert!(normalize_preview_path(raw).is_err(), "{raw:?} accepted");
        }
    }

    #[test]
    fn repo_declarations_with_unsafe_paths_are_skipped() {
        let (specs, w) = parse_previews(&json!({
            "evil": {"port_env": "PORT", "path": "//attacker.example/"},
            "bs": {"offset": 1, "path": "/\\attacker.example/"},
            "ok": {"offset": 2, "path": "/app#/x"},
        }));
        assert!(w.is_empty(), "{w:?}");
        let (out, warn) = resolve_previews(&specs, Some(&lease()), &port_env_offsets(None), false);
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].path, "/app#/x");
        assert_eq!(warn.len(), 2, "{warn:?}");
        assert!(warn.iter().all(|w| w.contains("path")), "{warn:?}");
    }

    fn lease() -> Lease {
        Lease {
            start: 20010,
            end: 20019,
            task_id: "t".into(),
            session: "s".into(),
            owner_pid: None,
            created_at: 0,
        }
    }

    #[test]
    fn resolves_ports_from_the_lease() {
        let (specs, w) = parse_previews(&json!({
            "web": {"port_env": "PORT", "path": "app"},
            "api": {"port_env": "API_PORT", "label": "API", "scheme": "https"},
            "docs": {"offset": 5},
            "sb": "VIBEKE_PORT_7",
            "bad": 3,
            "none": {"path": "/"},
        }));
        assert_eq!(w.len(), 2, "{w:?}");
        let offsets = port_env_offsets(Some(&json!({"env": {"API_PORT": 1, "STORY": "2"}})));
        assert!(offsets.contains(&("PORT".into(), 0)));
        assert!(offsets.contains(&("STORY".into(), 2)));
        let (r, w) = resolve_previews(&specs, Some(&lease()), &offsets, false);
        assert!(w.is_empty(), "{w:?}");
        let by = |n: &str| r.iter().find(|x| x.name == n).unwrap().clone();
        assert_eq!(by("web").port, 20010);
        assert_eq!(by("web").path, "/app");
        assert_eq!(by("web").label, "web");
        assert_eq!(by("api").port, 20011);
        assert_eq!(by("api").scheme, "https");
        assert_eq!(by("api").label, "API");
        assert_eq!(by("docs").port, 20015);
        assert_eq!(by("sb").port, 20017);
    }

    #[test]
    fn tls_origin_per_entry_and_table_default() {
        let table = json!({
            "tls_origin": true,
            "web": {"port_env": "PORT", "tls_origin": true},
            "api": {"offset": 1, "tls_origin": false},
            "plain": {"offset": 2},
        });
        assert_eq!(previews_tls_default(&table), Some(true));
        assert_eq!(previews_tls_default(&json!({"web": "PORT"})), None);
        // The table-wide key is not an entry (and not a warning).
        let (specs, w) = parse_previews(&table);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(specs.len(), 3);
        let (r, w) = resolve_previews(&specs, Some(&lease()), &port_env_offsets(None), false);
        assert!(w.is_empty(), "{w:?}");
        let by = |n: &str| r.iter().find(|x| x.name == n).unwrap().tls_origin;
        assert_eq!(
            (by("web"), by("api"), by("plain")),
            (Some(true), Some(false), None)
        );
    }

    #[test]
    fn repo_config_stays_inside_the_lease() {
        let (specs, _) = parse_previews(&json!({
            "ssh": {"port": 22},
            "far": {"offset": 10},
            "unknown": {"port_env": "DB_PORT"},
            "dup": {"port_env": "VIBEKE_PORT_BASE"},
            "web": {"port_env": "PORT"},
            "ws": {"port_env": "PORT", "path": "/a b"},
        }));
        let (r, w) = resolve_previews(&specs, Some(&lease()), &port_env_offsets(None), false);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(r[0].port, 20010);
        let all = w.join("\n");
        assert!(all.contains("not port = 22"), "{all}");
        assert!(all.contains("outside the leased block"), "{all}");
        assert!(all.contains("DB_PORT is not a leased port"), "{all}");
        assert!(all.contains("already used"), "{all}");
        // User-supplied config may name absolute ports.
        let (r, _) = resolve_previews(&specs, Some(&lease()), &port_env_offsets(None), true);
        assert!(r.iter().any(|x| x.port == 22));
        // No lease: nothing but warnings for leased ports.
        let (r, w) = resolve_previews(&specs, None, &port_env_offsets(None), false);
        assert!(r.is_empty());
        assert!(w.iter().any(|x| x.contains("no port lease")));
    }
}
