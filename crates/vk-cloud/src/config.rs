//! The `[cloud]` config table (spec 17 §9):
//!
//! ```toml
//! [cloud]
//! default_provider = "sprites"
//! idle_suspend_after = "30m"     # suspend boxes with no live session ("" = never)
//! idle_destroy_after = ""        # destroy idle boxes without unsynced work ("" = never)
//! on_task_close = "ask"          # ask | suspend | destroy | keep
//! after_bring_back = "suspend"   # keep | suspend | destroy
//!
//! [cloud.sprites]
//! credential = { env = "SPRITES_TOKEN" }   # optional; default keychain item vibeke/cloud/sprites
//! url_auth = "sprite"                      # sprite (org only) | public
//!
//! [cloud.e2b]
//! template = "base"
//! timeout = "1h"
//! auto_pause = true
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Exactly one credential source (same shape as the assistant's `credential`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CredentialRef {
    pub env: Option<String>,
    pub file: Option<String>,
    pub keychain: Option<String>,
}

/// `[cloud.<provider>]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub credential: Option<CredentialRef>,
    /// API base URL override (tests, self-hosted endpoints).
    pub api_url: Option<String>,
    /// Template or image (E2B template id or alias).
    pub template: Option<String>,
    /// Lifetime hint (E2B sandbox timeout), a duration such as `1h`.
    pub timeout: Option<String>,
    pub auto_pause: Option<bool>,
    /// Sprites URL access: `sprite` (org members) or `public`.
    pub url_auth: Option<String>,
}

impl ProviderConfig {
    pub fn timeout_s(&self) -> u64 {
        self.timeout
            .as_deref()
            .and_then(parse_duration)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// `[cloud]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudConfig {
    pub default_provider: String,
    pub idle_suspend_after: String,
    pub idle_destroy_after: String,
    pub on_task_close: String,
    pub after_bring_back: String,
    #[serde(flatten)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

impl Default for CloudConfig {
    fn default() -> Self {
        CloudConfig {
            default_provider: "sprites".into(),
            idle_suspend_after: "30m".into(),
            idle_destroy_after: String::new(),
            on_task_close: "ask".into(),
            after_bring_back: "suspend".into(),
            providers: BTreeMap::new(),
        }
    }
}

impl CloudConfig {
    /// Parse `[cloud]` from the user config's extra tables; problems fall back to defaults
    /// with the error returned for a warning.
    pub fn from_config(cfg: &vk_config::Config) -> (CloudConfig, Option<String>) {
        match cfg.extra.get("cloud") {
            None => (CloudConfig::default(), None),
            Some(v) => match v.clone().try_into::<CloudConfig>() {
                Ok(c) => (c, None),
                Err(e) => (CloudConfig::default(), Some(e.to_string())),
            },
        }
    }

    /// Load from the user's config file.
    pub fn load() -> CloudConfig {
        let cfg = vk_config::Config::load(vk_config::config_path())
            .map(|(c, _)| c)
            .unwrap_or_default();
        let (c, e) = CloudConfig::from_config(&cfg);
        if let Some(e) = e {
            tracing::warn!(error = %e, "invalid [cloud] config; using defaults");
        }
        c
    }

    pub fn provider(&self, id: &str) -> ProviderConfig {
        self.providers.get(id).cloned().unwrap_or_default()
    }

    pub fn idle_suspend(&self) -> Option<Duration> {
        parse_duration(&self.idle_suspend_after)
    }

    pub fn idle_destroy(&self) -> Option<Duration> {
        parse_duration(&self.idle_destroy_after)
    }
}

/// `30s`, `30m`, `2h`, `1d`; empty or `0` = `None`.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n.parse().ok()?;
    let secs = match unit.trim() {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => return None,
    };
    (secs > 0).then(|| Duration::from_secs(secs))
}

pub fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest),
        None => PathBuf::from(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("0"), None);
        assert_eq!(parse_duration("5x"), None);
    }

    #[test]
    fn parses_provider_tables() {
        let v: toml::Value = toml::from_str(
            "default_provider = \"e2b\"\n[sprites]\ncredential = { env = \"T\" }\n[e2b]\ntimeout = \"1h\"\n",
        )
        .unwrap();
        let c: CloudConfig = v.try_into().unwrap();
        assert_eq!(c.default_provider, "e2b");
        assert_eq!(c.provider("e2b").timeout_s(), 3600);
        assert_eq!(
            c.provider("sprites").credential.unwrap().env.as_deref(),
            Some("T")
        );
        assert_eq!(c.on_task_close, "ask");
    }
}
