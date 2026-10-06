//! `[assistant]` configuration (14 §5–6). User-level only; off by default.
//!
//! ```toml
//! [assistant]
//! enabled = true
//! default_profile = "interactive"
//!
//! [assistant.connections.primary]
//! adapter = "anthropic"                     # anthropic | openai_compatible | ollama
//! credential = { env = "VIBEKE_ASSISTANT_API_KEY" }   # or { file = "~/.config/vibeke/assistant.key" }
//!
//! [assistant.profiles.interactive]
//! connection = "primary"
//! model = "claude-haiku-4-5-20251001"
//! ```

use crate::{AssistError, Category, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-haiku-4-5-20251001";
pub const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com";
pub const OPENAI_ENDPOINT: &str = "https://api.openai.com";
pub const OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AssistConfig {
    /// Master switch. Off by default: no provider is contacted while false.
    pub enabled: bool,
    pub default_profile: String,
    pub max_concurrent_requests: usize,
    pub max_queued_requests: usize,
    /// Provider attempts per UTC day on this coordinator (14 §8).
    pub daily_request_limit: u64,
    /// Input + output tokens per UTC day (reserved as estimate + max output before dispatch).
    pub daily_token_limit: u64,
    /// Optional estimated-cost cap per UTC day. Requires known pricing for the model.
    pub daily_cost_limit_usd: Option<f64>,
    /// Sliding one-minute window of provider attempts.
    pub requests_per_minute: u32,
    pub request_timeout_seconds: u64,
    pub result_retention_hours: u64,
    /// How long an unconfirmed preview stays sendable (held in memory only).
    pub preview_ttl_seconds: u64,
    /// Operations that skip the per-request preview confirmation. Workspace consent must also
    /// list the operation in its own `auto_send`.
    pub auto_send: Vec<String>,
    pub connections: BTreeMap<String, Connection>,
    pub profiles: BTreeMap<String, Profile>,
}

impl Default for AssistConfig {
    fn default() -> Self {
        AssistConfig {
            enabled: false,
            default_profile: "interactive".into(),
            max_concurrent_requests: 2,
            max_queued_requests: 16,
            daily_request_limit: 100,
            daily_token_limit: 200_000,
            daily_cost_limit_usd: None,
            requests_per_minute: 6,
            request_timeout_seconds: 60,
            result_retention_hours: 24,
            preview_ttl_seconds: 600,
            auto_send: vec![],
            connections: BTreeMap::new(),
            profiles: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Adapter {
    Anthropic,
    #[serde(alias = "openai")]
    OpenaiCompatible,
    Ollama,
}

impl Adapter {
    pub fn as_str(self) -> &'static str {
        match self {
            Adapter::Anthropic => "anthropic",
            Adapter::OpenaiCompatible => "openai_compatible",
            Adapter::Ollama => "ollama",
        }
    }
    pub fn default_endpoint(self) -> &'static str {
        match self {
            Adapter::Anthropic => ANTHROPIC_ENDPOINT,
            Adapter::OpenaiCompatible => OPENAI_ENDPOINT,
            Adapter::Ollama => OLLAMA_ENDPOINT,
        }
    }
    pub fn needs_credential(self) -> bool {
        !matches!(self, Adapter::Ollama)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub adapter: Adapter,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub credential: Option<Credential>,
}

/// Exactly one credential source. No ambient fallback: an unresolvable reference is an error.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Credential {
    /// Name of an environment variable on the coordinator.
    pub env: Option<String>,
    /// A key file the user created for Vibeke (0600, owned by the user).
    pub file: Option<String>,
    /// OS keychain item. Accepted in config, not implemented yet (reported as unsupported).
    pub keychain: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    pub connection: String,
    pub model: String,
    pub max_input_tokens: u64,
    pub max_input_bytes: usize,
    pub max_output_tokens: u64,
    /// Price overrides (USD per million tokens) for models without built-in pricing.
    pub input_usd_per_mtok: Option<f64>,
    pub output_usd_per_mtok: Option<f64>,
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            connection: String::new(),
            model: String::new(),
            max_input_tokens: 12_000,
            max_input_bytes: 65_536,
            max_output_tokens: 1024,
            input_usd_per_mtok: None,
            output_usd_per_mtok: None,
        }
    }
}

/// A profile resolved to its connection and a validated endpoint.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Resolved {
    pub profile_id: String,
    pub profile: Profile,
    pub connection_id: String,
    pub connection: Connection,
    pub endpoint: String,
    /// Digest of adapter + endpoint: consent grants are bound to it (14 §6).
    pub fingerprint: String,
}

impl Resolved {
    pub fn endpoint_host(&self) -> String {
        reqwest::Url::parse(&self.endpoint)
            .ok()
            .and_then(|u| {
                u.host_str().map(|h| {
                    format!(
                        "{h}{}",
                        u.port().map(|p| format!(":{p}")).unwrap_or_default()
                    )
                })
            })
            .unwrap_or_default()
    }

    /// Estimated cost in USD, `None` when pricing is unknown (never zero by default).
    pub fn cost(&self, input_tokens: u64, output_tokens: u64) -> Option<f64> {
        let (i, o) = self.prices()?;
        Some((input_tokens as f64 * i + output_tokens as f64 * o) / 1_000_000.0)
    }

    pub fn prices(&self) -> Option<(f64, f64)> {
        match (
            self.profile.input_usd_per_mtok,
            self.profile.output_usd_per_mtok,
        ) {
            (Some(i), Some(o)) => Some((i, o)),
            _ if self.connection.adapter == Adapter::Anthropic => {
                builtin_price(&self.profile.model)
            }
            _ => None,
        }
    }
}

/// First-party Anthropic list prices (USD per MTok input/output), checked 2026-10-06.
pub fn builtin_price(model: &str) -> Option<(f64, f64)> {
    match model {
        "claude-haiku-4-5" | "claude-haiku-4-5-20251001" => Some((1.0, 5.0)),
        "claude-sonnet-5-5" => Some((2.0, 10.0)),
        "claude-opus-5-5" => Some((4.0, 20.0)),
        _ => None,
    }
}

pub fn fingerprint(adapter: Adapter, endpoint: &str) -> String {
    blake3::hash(format!("{}\n{endpoint}", adapter.as_str()).as_bytes()).to_hex()[..16].to_string()
}

/// A content-free description of a configuration parse error. Serde messages quote offending
/// values (`invalid type: string "sk-…"`), so only the unknown-key name — a key the user typed,
/// never a value — is kept, and only when it looks like an identifier.
pub fn describe_serde(e: &serde_json::Error) -> String {
    let m = e.to_string();
    if let Some(rest) = m.strip_prefix("unknown field `")
        && let Some((key, _)) = rest.split_once('`')
        && !key.is_empty()
        && key.len() <= 64
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return format!("unknown key `{key}`");
    }
    if m.starts_with("unknown variant") {
        return "a value is not one of the allowed choices (for example adapter = \"anthropic\" | \"openai_compatible\" | \"ollama\")".into();
    }
    if m.starts_with("missing field") {
        return "a required key is missing (connection, adapter or model)".into();
    }
    "a value has the wrong type or format (credentials must be { env = \"NAME\" } or { file = \"PATH\" }, never the key itself)".into()
}

fn nc(msg: impl Into<String>) -> AssistError {
    AssistError::new(Category::NotConfigured, msg)
}

impl AssistConfig {
    /// Parse the `[assistant]` table (as JSON). Unknown keys are errors, not silently ignored.
    /// The error never echoes a configured value (an inline credential string, say): only an
    /// unknown key's name or a generic type/format complaint (14 §8, 09 §9.3a).
    pub fn from_json(v: serde_json::Value) -> std::result::Result<Self, String> {
        serde_json::from_value(v).map_err(|e| format!("[assistant]: {}", describe_serde(&e)))
    }

    pub fn auto_send_allows(&self, op: &str) -> bool {
        self.auto_send.iter().any(|o| o == op)
    }

    /// Resolve a profile (default when `None`) to its connection. Misconfiguration fails
    /// visibly; there is no fallback to another profile, connection or provider.
    pub fn resolve(&self, profile: Option<&str>) -> Result<Resolved> {
        let pid = profile.unwrap_or(&self.default_profile);
        let p = self
            .profiles
            .get(pid)
            .ok_or_else(|| nc(format!("no profile `{pid}` in [assistant.profiles]")))?;
        if p.model.trim().is_empty() {
            return Err(nc(format!("profile `{pid}` has no model")));
        }
        if p.max_output_tokens == 0 || p.max_input_bytes == 0 || p.max_input_tokens == 0 {
            return Err(nc(format!("profile `{pid}` has a zero limit")));
        }
        let c = self.connections.get(&p.connection).ok_or_else(|| {
            nc(format!(
                "profile `{pid}` names unknown connection `{}`",
                p.connection
            ))
        })?;
        let endpoint = c
            .endpoint
            .clone()
            .unwrap_or_else(|| c.adapter.default_endpoint().to_string());
        let endpoint = validate_endpoint(&endpoint)?;
        Ok(Resolved {
            profile_id: pid.to_string(),
            profile: p.clone(),
            connection_id: p.connection.clone(),
            connection: c.clone(),
            fingerprint: fingerprint(c.adapter, &endpoint),
            endpoint,
        })
    }
}

/// HTTPS is required except for explicitly configured loopback endpoints (14 §10).
pub fn validate_endpoint(e: &str) -> Result<String> {
    // Messages name the rule, never the configured value (it could carry a token).
    let u = reqwest::Url::parse(e).map_err(|_| nc("the endpoint is not a valid URL"))?;
    if !u.username().is_empty() || u.password().is_some() {
        return Err(nc("endpoint URLs must not carry credentials"));
    }
    let loopback = u.host_str().is_some_and(is_loopback_host);
    match u.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => {
            return Err(nc(
                "plain HTTP is only allowed for loopback endpoints; use HTTPS or an SSH tunnel",
            ));
        }
        _ => return Err(nc("unsupported endpoint scheme (use https://)")),
    }
    Ok(u.as_str().trim_end_matches('/').to_string())
}

fn is_loopback_host(h: &str) -> bool {
    let bare = h.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    bare.eq_ignore_ascii_case("localhost")
}

/// Where a credential comes from, for status output (never the value).
pub fn credential_source(c: &Connection) -> String {
    match &c.credential {
        Some(Credential { env: Some(e), .. }) => format!("env:{e}"),
        Some(Credential { file: Some(f), .. }) => format!("file:{f}"),
        Some(Credential {
            keychain: Some(k), ..
        }) => format!("keychain:{k}"),
        _ if !c.adapter.needs_credential() => "none".into(),
        _ => "missing".into(),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Harness and cloud CLI credential locations Vibeke must never read for the assistant
/// (14 §6: no ambient credentials; the user's Claude/Codex logins are not ours).
const FORBIDDEN: &[&str] = &[
    ".claude",
    ".claude.json",
    ".codex",
    ".config/claude",
    ".config/anthropic",
    ".config/openai",
    ".config/gcloud",
    ".gemini",
    ".pi",
    ".omp",
    ".aws",
    ".ssh",
    ".netrc",
    ".docker",
];

pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

fn forbidden(path: &Path) -> bool {
    let h = home();
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    FORBIDDEN.iter().any(|f| {
        let bad = h.join(f);
        let bad_canon = std::fs::canonicalize(&bad).unwrap_or(bad.clone());
        path.starts_with(&bad) || canon.starts_with(&bad) || canon.starts_with(&bad_canon)
    })
}

/// Resolve the API key for a connection. `Ok(None)` only for adapters without credentials
/// (local Ollama) and no credential configured.
pub fn resolve_credential(c: &Connection) -> Result<Option<String>> {
    let Some(cred) = &c.credential else {
        if c.adapter.needs_credential() {
            return Err(nc(format!(
                "connection needs a credential reference ({} adapter)",
                c.adapter.as_str()
            )));
        }
        return Ok(None);
    };
    let n = [
        cred.env.is_some(),
        cred.file.is_some(),
        cred.keychain.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if n != 1 {
        return Err(nc(
            "credential must name exactly one of env, file, keychain",
        ));
    }
    if let Some(name) = &cred.env {
        let v = std::env::var(name).unwrap_or_default();
        let v = v.trim();
        if v.is_empty() {
            return Err(AssistError::new(
                Category::AuthenticationFailed,
                "the configured credential environment variable is not set on the coordinator",
            ));
        }
        return Ok(Some(v.to_string()));
    }
    if cred.keychain.is_some() {
        return Err(AssistError::new(
            Category::UnsupportedCapability,
            "keychain credentials are not implemented yet; use env or file",
        ));
    }
    let path = expand(cred.file.as_deref().unwrap_or_default());
    if forbidden(&path) {
        return Err(AssistError::new(
            Category::PermissionDenied,
            "refusing to read a harness or cloud CLI credential store; create a dedicated key file",
        ));
    }
    // Open once (never following a final symlink, never blocking on a FIFO) and validate and
    // read through that same descriptor, so the file can't be swapped between the check and
    // the read. Messages never include the configured path.
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                AssistError::new(
                    Category::PermissionDenied,
                    "the credential file must not be a symbolic link",
                )
            } else {
                AssistError::new(
                    Category::AuthenticationFailed,
                    "the configured credential file is not readable",
                )
            }
        })?;
    let meta = f.metadata().map_err(|_| {
        AssistError::new(
            Category::AuthenticationFailed,
            "the configured credential file is not readable",
        )
    })?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o077 != 0 || meta.len() > 4096 {
        return Err(AssistError::new(
            Category::PermissionDenied,
            "the credential file must be a small regular file owned by you with mode 0600",
        ));
    }
    let mut text = String::new();
    (&mut f).take(4097).read_to_string(&mut text).map_err(|_| {
        AssistError::new(Category::AuthenticationFailed, "credential file unreadable")
    })?;
    let key = text.lines().next().unwrap_or("").trim().to_string();
    if key.is_empty() {
        return Err(AssistError::new(
            Category::AuthenticationFailed,
            "credential file is empty",
        ));
    }
    Ok(Some(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(endpoint: Option<&str>) -> AssistConfig {
        let mut v = json!({
            "enabled": true,
            "connections": {"primary": {"adapter": "anthropic", "credential": {"env": "VK_TEST_NOPE"}}},
            "profiles": {"interactive": {"connection": "primary", "model": DEFAULT_ANTHROPIC_MODEL}},
        });
        if let Some(e) = endpoint {
            v["connections"]["primary"]["endpoint"] = json!(e);
        }
        AssistConfig::from_json(v).unwrap()
    }

    #[test]
    fn disabled_by_default() {
        let c = AssistConfig::default();
        assert!(!c.enabled);
        assert!(c.resolve(None).is_err());
    }

    #[test]
    fn unknown_keys_rejected() {
        assert!(AssistConfig::from_json(json!({"enabeld": true})).is_err());
    }

    #[test]
    fn endpoint_rules() {
        assert_eq!(
            cfg(None).resolve(None).unwrap().endpoint,
            "https://api.anthropic.com"
        );
        assert!(cfg(Some("http://127.0.0.1:9999")).resolve(None).is_ok());
        assert!(cfg(Some("http://[::1]:9999")).resolve(None).is_ok());
        assert!(cfg(Some("http://localhost:9")).resolve(None).is_ok());
        let e = cfg(Some("http://example.com")).resolve(None).unwrap_err();
        assert_eq!(e.category, Category::NotConfigured);
        assert!(cfg(Some("https://u:p@example.com")).resolve(None).is_err());
        assert!(cfg(Some("file:///etc/passwd")).resolve(None).is_err());
    }

    #[test]
    fn fingerprint_tracks_endpoint() {
        let a = cfg(Some("http://127.0.0.1:1")).resolve(None).unwrap();
        let b = cfg(Some("http://127.0.0.1:2")).resolve(None).unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn pricing_known_and_unknown() {
        let r = cfg(None).resolve(None).unwrap();
        let c = r.cost(1_000_000, 1_000_000).unwrap();
        assert!((c - 6.0).abs() < 1e-9);
        assert_eq!(builtin_price("claude-opus-5-5"), Some((4.0, 20.0)));
        assert_eq!(builtin_price("llama3"), None);
    }

    #[test]
    fn missing_env_credential_is_an_error_without_fallback() {
        let c = Connection {
            adapter: Adapter::Anthropic,
            endpoint: None,
            credential: Some(Credential {
                env: Some("VK_TEST_DEFINITELY_UNSET".into()),
                ..Default::default()
            }),
        };
        assert_eq!(
            resolve_credential(&c).unwrap_err().category,
            Category::AuthenticationFailed
        );
        let none = Connection {
            credential: None,
            ..c.clone()
        };
        assert_eq!(
            resolve_credential(&none).unwrap_err().category,
            Category::NotConfigured
        );
        let ollama = Connection {
            adapter: Adapter::Ollama,
            credential: None,
            endpoint: None,
        };
        assert_eq!(resolve_credential(&ollama).unwrap(), None);
    }

    #[test]
    fn credential_file_rules() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("key");
        std::fs::write(&f, "sk-test-123\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        let c = Connection {
            adapter: Adapter::OpenaiCompatible,
            endpoint: None,
            credential: Some(Credential {
                file: Some(f.to_string_lossy().into()),
                ..Default::default()
            }),
        };
        assert_eq!(
            resolve_credential(&c).unwrap_err().category,
            Category::PermissionDenied
        );
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            resolve_credential(&c).unwrap().as_deref(),
            Some("sk-test-123")
        );
    }

    #[test]
    fn parse_errors_never_echo_values() {
        let sentinel = "sk-ant-SENTINEL-inline-0123456789";
        for v in [
            json!({"connections": {"p": {"adapter": "anthropic", "credential": sentinel}}}),
            json!({"connections": {"p": {"adapter": sentinel}}}),
            json!({"connections": {"p": {"adapter": "anthropic", "endpoint": 7}}, "enabled": sentinel}),
        ] {
            let e = AssistConfig::from_json(v).unwrap_err();
            assert!(!e.contains("SENTINEL"), "{e}");
        }
        let e = AssistConfig::from_json(json!({"enabeld": true})).unwrap_err();
        assert!(e.contains("unknown key `enabeld`"), "{e}");
        let e = cfg(Some("https://example.com/v1?key=SENTINEL\u{7f}%"))
            .resolve(None)
            .err();
        assert!(e.is_none_or(|e| !e.message.contains("SENTINEL")));
        let e = cfg(Some("ftp://SENTINEL@host")).resolve(None).unwrap_err();
        assert!(!e.message.contains("SENTINEL"), "{}", e.message);
    }

    #[test]
    fn credential_file_symlinks_refused_and_paths_not_echoed() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real-SENTINEL");
        std::fs::write(&real, "sk-test-123\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = d.path().join("link-SENTINEL");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let conn = |p: &Path| Connection {
            adapter: Adapter::OpenaiCompatible,
            endpoint: None,
            credential: Some(Credential {
                file: Some(p.to_string_lossy().into()),
                ..Default::default()
            }),
        };
        let e = resolve_credential(&conn(&link)).unwrap_err();
        assert_eq!(e.category, Category::PermissionDenied);
        assert!(!e.message.contains("SENTINEL"), "{}", e.message);
        let e = resolve_credential(&conn(&d.path().join("missing-SENTINEL"))).unwrap_err();
        assert!(!e.message.contains("SENTINEL"), "{}", e.message);
        // A directory or FIFO at the path is refused without blocking.
        let fifo = d.path().join("fifo");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: valid NUL-terminated path.
        unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(
            resolve_credential(&conn(&fifo)).unwrap_err().category,
            Category::PermissionDenied
        );
        assert_eq!(
            resolve_credential(&conn(&real)).unwrap().as_deref(),
            Some("sk-test-123")
        );
    }

    #[test]
    fn harness_credential_stores_refused() {
        let c = Connection {
            adapter: Adapter::Anthropic,
            endpoint: None,
            credential: Some(Credential {
                file: Some("~/.claude/.credentials.json".into()),
                ..Default::default()
            }),
        };
        assert_eq!(
            resolve_credential(&c).unwrap_err().category,
            Category::PermissionDenied
        );
        let codex = Connection {
            credential: Some(Credential {
                file: Some("~/.codex/auth.json".into()),
                ..Default::default()
            }),
            ..c
        };
        assert_eq!(
            resolve_credential(&codex).unwrap_err().category,
            Category::PermissionDenied
        );
    }
}
