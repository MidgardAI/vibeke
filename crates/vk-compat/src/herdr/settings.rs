//! Per-plugin runtime settings from the `[plugins]` section of `config.toml` (07 §7.7).
//!
//! ```toml
//! [plugins]                       # defaults for every plugin
//! max_concurrent = 4              # running action/hook invocations per plugin
//! log_max_bytes = 65536           # kept per stdout/stderr stream of one invocation
//! log_max_lines = 1000
//! log_max_records = 25            # invocation log records kept per plugin
//!
//! [plugins."acme.tool"]           # one plugin (quote ids: they contain dots)
//! isolate = "sandbox"             # restricted legacy mode (default "host")
//! network = false                 # a sandboxed plugin's network (default off)
//! max_concurrent = 2
//! ```
//!
//! Values outside their range are clamped; unknown keys are ignored here (the config loader
//! preserves the section verbatim).

/// Running invocations per plugin when nothing is configured.
pub const DEFAULT_MAX_CONCURRENT: usize = 4;
/// Pending event-hook invocations kept per plugin while all slots are busy.
pub const MAX_QUEUED: usize = 16;
pub const DEFAULT_LOG_BYTES: usize = 64 * 1024;
pub const DEFAULT_LOG_LINES: usize = 1000;
pub const DEFAULT_LOG_RECORDS: usize = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolate {
    /// Trusted host execution (full legacy compatibility).
    Host,
    /// Restricted legacy mode under the `sandbox` level (13): not fully compatible.
    Sandbox,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSettings {
    pub max_concurrent: usize,
    pub log_max_bytes: usize,
    pub log_max_lines: usize,
    pub log_max_records: usize,
    pub isolate: Isolate,
    /// Network for a sandboxed plugin (ignored on the host).
    pub network: bool,
    /// Messages for values that were ignored (shown by `plugin.list`).
    pub warnings: Vec<String>,
}

impl Default for PluginSettings {
    fn default() -> Self {
        PluginSettings {
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            log_max_bytes: DEFAULT_LOG_BYTES,
            log_max_lines: DEFAULT_LOG_LINES,
            log_max_records: DEFAULT_LOG_RECORDS,
            isolate: Isolate::Host,
            network: false,
            warnings: vec![],
        }
    }
}

impl PluginSettings {
    /// Settings for plugin `id` from the `[plugins]` value (global scalars, then the plugin's
    /// own table on top).
    pub fn from_toml(plugins: Option<&toml::Value>, id: &str) -> PluginSettings {
        let mut s = PluginSettings::default();
        let Some(t) = plugins.and_then(toml::Value::as_table) else {
            return s;
        };
        let scalars = |s: &mut PluginSettings, t: &toml::map::Map<String, toml::Value>| {
            let num = |k: &str, lo: i64, hi: i64, s: &mut PluginSettings| -> Option<usize> {
                match t.get(k)? {
                    toml::Value::Integer(n) if (lo..=hi).contains(n) => Some(*n as usize),
                    toml::Value::Integer(n) => {
                        let c = (*n).clamp(lo, hi);
                        s.warnings
                            .push(format!("{k} = {n} is outside {lo}..={hi}; using {c}"));
                        Some(c as usize)
                    }
                    _ => {
                        s.warnings.push(format!("{k} must be an integer"));
                        None
                    }
                }
            };
            if let Some(n) = num("max_concurrent", 1, 64, s) {
                s.max_concurrent = n;
            }
            if let Some(n) = num("log_max_bytes", 1024, 16 * 1024 * 1024, s) {
                s.log_max_bytes = n;
            }
            if let Some(n) = num("log_max_lines", 10, 100_000, s) {
                s.log_max_lines = n;
            }
            if let Some(n) = num("log_max_records", 1, 1000, s) {
                s.log_max_records = n;
            }
            match t.get("isolate").and_then(toml::Value::as_str) {
                Some("host") => s.isolate = Isolate::Host,
                Some("sandbox") => s.isolate = Isolate::Sandbox,
                Some(o) => s.warnings.push(format!(
                    "isolate = \"{o}\" is not `host` or `sandbox`; using host"
                )),
                None => {}
            }
            if let Some(b) = t.get("network").and_then(toml::Value::as_bool) {
                s.network = b;
            }
        };
        scalars(&mut s, t);
        if let Some(own) = t.get(id).and_then(toml::Value::as_table) {
            scalars(&mut s, own);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> toml::Value {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn defaults_and_overrides() {
        let d = PluginSettings::from_toml(None, "a.b");
        assert_eq!(d.max_concurrent, 4);
        assert_eq!(d.isolate, Isolate::Host);
        assert!(!d.network);
        let v = parse(
            "[plugins]\nmax_concurrent = 3\nlog_max_lines = 50\n[plugins.\"a.b\"]\nisolate = \"sandbox\"\nmax_concurrent = 1\nnetwork = true\n[plugins.\"c.d\"]\nmax_concurrent = 9\n",
        );
        let p = v.get("plugins");
        let a = PluginSettings::from_toml(p, "a.b");
        assert_eq!(
            (a.max_concurrent, a.log_max_lines, a.isolate, a.network),
            (1, 50, Isolate::Sandbox, true)
        );
        let other = PluginSettings::from_toml(p, "e.f");
        assert_eq!((other.max_concurrent, other.isolate), (3, Isolate::Host));
        assert_eq!(PluginSettings::from_toml(p, "c.d").max_concurrent, 9);
    }

    #[test]
    fn bad_values_are_clamped_and_reported() {
        let v = parse("[plugins]\nmax_concurrent = 0\nlog_max_bytes = \"x\"\nisolate = \"vm\"\n");
        let s = PluginSettings::from_toml(v.get("plugins"), "a.b");
        assert_eq!(s.max_concurrent, 1);
        assert_eq!(s.log_max_bytes, DEFAULT_LOG_BYTES);
        assert_eq!(s.isolate, Isolate::Host);
        assert_eq!(s.warnings.len(), 3, "{:?}", s.warnings);
    }
}
