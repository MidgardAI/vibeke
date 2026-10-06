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
}

/// A task preview with its port resolved from the lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedPreview {
    pub name: String,
    pub port: u16,
    pub path: String,
    pub label: String,
    pub scheme: String,
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
        let mut path = s.path.clone().unwrap_or_else(|| "/".into());
        if !path.starts_with('/') {
            path.insert(0, '/');
        }
        if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
            warn.push(format!(
                "preview {}: path must not contain whitespace",
                s.name
            ));
            continue;
        }
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
            from,
        });
    }
    (out, warn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
