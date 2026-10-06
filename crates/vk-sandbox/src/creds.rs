//! Credential projection (13 §8, 09): each harness gets exactly the model credentials it needs
//! inside the box and nothing else.
//!
//! The harness's config dir is relocated to an **ephemeral home** inside the pane's private dir
//! (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CODING_AGENT_DIR`), seeded with copies of the
//! non-secret config the user has (settings with Vibeke's hooks, instructions, extensions) plus
//! the credential file, which is made read-only (0400 and a Seatbelt/bind-mount write deny).
//! Config files are **allow-listed, not copied verbatim**: Claude `settings.json` keeps only
//! [`CLAUDE_SETTINGS_KEYS`] (no `env`, credential helpers or MCP servers), Codex `config.toml`
//! only [`CODEX_KEYS`] with MCP `env`/headers/tokens stripped, pi's JSON loses every
//! secret-named key. Ephemeral homes are created and written without following symlinks
//! ([`crate::fsafe`]).
//! The box can therefore never modify the host's harness config (hooks there would run on the
//! host later) or the host's credential files.
//!
//! Secret *values* only ever live in the projected files and in the spawn env (which reaches the
//! holder through its 0600 spec file). [`Projection`]'s `Debug` and [`Projection::summary`] name
//! what was projected without values, so events and logs can mention it safely.

use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[path = "creds_declared.rs"]
mod declared;
pub use declared::{
    DeclaredAuth, env_refused, file_refused, project_declared, store_claude_setup_token,
};

/// Harnesses with built-in projection rules (manifest `[auth]`, 04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HarnessAuth {
    Claude,
    Codex,
    Pi,
    Omp,
}

impl HarnessAuth {
    pub fn from_id(s: &str) -> Option<HarnessAuth> {
        Some(match s {
            "claude" => HarnessAuth::Claude,
            "codex" => HarnessAuth::Codex,
            "pi" => HarnessAuth::Pi,
            "omp" => HarnessAuth::Omp,
            _ => return None,
        })
    }
    pub fn id(&self) -> &'static str {
        match self {
            HarnessAuth::Claude => "claude",
            HarnessAuth::Codex => "codex",
            HarnessAuth::Pi => "pi",
            HarnessAuth::Omp => "omp",
        }
    }
}

/// Provider API-key variables pi/omp (and API-key Claude/Codex users) may need.
pub const PROVIDER_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENROUTER_API_KEY",
    "GROQ_API_KEY",
    "MISTRAL_API_KEY",
    "XAI_API_KEY",
    "DEEPSEEK_API_KEY",
];

/// Where the host keeps things, and where the projection goes.
#[derive(Debug, Clone)]
pub struct ProjectionInput<'a> {
    pub home: &'a Path,
    /// Host env the server would normally hand to panes.
    pub host_env: &'a [(String, String)],
    /// Vibeke's credential store (`<state>/credentials`): `claude-oauth-token` from
    /// `claude setup-token` (0600, never group/world readable).
    pub vibeke_credentials: &'a Path,
    /// Pane private dir; ephemeral homes go under `<private>/home/<harness>`.
    pub private_dir: &'a Path,
    /// Pre-accept the workspace trust dialog for `checkout` (only for yolo launches).
    pub trust_checkout: Option<&'a Path>,
    /// Allow Vibeke's own config dir overrides (tests): harness config roots on the host.
    pub claude_dir: Option<PathBuf>,
    pub codex_dir: Option<PathBuf>,
    pub pi_agent_dir: Option<PathBuf>,
}

#[derive(Default, Clone)]
pub struct Projection {
    /// Env additions for the contained process (values may be secret).
    pub env: Vec<(String, String)>,
    /// Paths to add to the read-only allowlist.
    pub read: Vec<PathBuf>,
    /// Paths to add to the read-write allowlist.
    pub write: Vec<PathBuf>,
    /// Credential files: readable, never writable.
    pub read_only_files: Vec<PathBuf>,
    /// Names of projected secrets (env names / file names), never values.
    pub names: Vec<String>,
    /// Secret values, for redaction of anything that might echo them.
    secrets: Vec<String>,
}

impl fmt::Debug for Projection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Projection")
            .field("env", &self.env.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .field("read_only_files", &self.read_only_files)
            .field("names", &self.names)
            .finish()
    }
}

impl Projection {
    /// What was projected, safe for events and the first-use notice (13 §8).
    pub fn summary(&self) -> Vec<String> {
        self.names.clone()
    }
    /// Replace every projected secret value in `s` with `[redacted]`.
    pub fn redact(&self, s: &str) -> String {
        let mut out = s.to_string();
        for v in &self.secrets {
            if v.len() >= 8 {
                out = out.replace(v.as_str(), "[redacted]");
            }
        }
        out
    }
    pub fn secret_count(&self) -> usize {
        self.secrets.len()
    }
    fn secret_env(&mut self, k: &str, v: String) {
        self.secrets.push(v.clone());
        self.env.retain(|(x, _)| x != k);
        self.env.push((k.to_string(), v));
        self.names.push(format!("env:{k}"));
    }
}

fn host_var<'a>(env: &'a [(String, String)], k: &str) -> Option<&'a str> {
    env.iter()
        .rev()
        .find(|(x, _)| x == k)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

/// The ephemeral home `<private>/<rel>`: created without following symlinks (the box can write
/// inside projected homes and may have swapped one for a link to a host dir).
fn ensure_home(private: &Path, rel: &Path) -> std::io::Result<PathBuf> {
    if !private.exists() {
        std::fs::create_dir_all(private)?;
        std::fs::set_permissions(private, std::fs::Permissions::from_mode(0o700))?;
    }
    crate::fsafe::ensure_dir_under(private, rel)
}

fn write_mode(p: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    crate::fsafe::write_nofollow(p, bytes, mode)
}

/// Copy a regular file or directory tree into the verified dir `dst_dir` as `name`. Symlinks
/// on either side are skipped/refused: a source link could point anywhere, a destination link
/// would redirect the copy onto the host.
fn copy_tree(src: &Path, dst_dir: &Path, name: &std::ffi::OsStr) -> std::io::Result<()> {
    let md = std::fs::symlink_metadata(src)?;
    if md.file_type().is_symlink() {
        return Ok(());
    }
    let dst = dst_dir.join(name);
    if md.is_dir() {
        let d = crate::fsafe::ensure_dir_under(dst_dir, Path::new(name))?;
        for e in std::fs::read_dir(src)?.flatten() {
            copy_tree(&e.path(), &d, &e.file_name())?;
        }
        return Ok(());
    }
    if md.is_file() {
        let bytes = std::fs::read(src)?;
        write_mode(&dst, &bytes, 0o600)?;
    }
    Ok(())
}

/// Copy one config file through a sanitizer (`None` from the sanitizer: not copied at all).
fn copy_sanitized(
    src: &Path,
    dst_dir: &Path,
    name: &str,
    sanitize: fn(&[u8]) -> Option<Vec<u8>>,
) -> std::io::Result<()> {
    let md = std::fs::symlink_metadata(src)?;
    if !md.is_file() {
        return Ok(());
    }
    match sanitize(&std::fs::read(src)?) {
        Some(bytes) => write_mode(&dst_dir.join(name), &bytes, 0o600),
        None => {
            tracing::warn!(path = %src.display(), "harness config not projected: could not parse it to strip secrets");
            Ok(())
        }
    }
}

/// Claude `settings.json` keys copied into the box (13 §8): behaviour and Vibeke's hooks. Not
/// copied: `env` (cloud/API tokens), `apiKeyHelper`/`awsAuthRefresh`/`awsCredentialExport`
/// (credential commands), MCP server definitions and anything unknown.
pub const CLAUDE_SETTINGS_KEYS: &[&str] = &[
    "hooks",
    "permissions",
    "model",
    "statusLine",
    "outputStyle",
    "includeCoAuthoredBy",
    "cleanupPeriodDays",
    "alwaysThinkingEnabled",
    "spinnerTipsEnabled",
    "disableAllHooks",
    "forceLoginMethod",
];

pub fn sanitize_claude_settings(bytes: &[u8]) -> Option<Vec<u8>> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let o = v.as_object()?;
    let kept: serde_json::Map<String, serde_json::Value> = o
        .iter()
        .filter(|(k, _)| CLAUDE_SETTINGS_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    serde_json::to_vec_pretty(&serde_json::Value::Object(kept)).ok()
}

/// Codex `config.toml` top-level keys copied into the box. `mcp_servers` and `model_providers`
/// are filtered per entry ([`CODEX_MCP_KEYS`], [`CODEX_PROVIDER_KEYS`]); `profiles` recursively.
/// Not copied: `shell_environment_policy` (its `set` holds env values), bearer tokens, HTTP
/// headers, MCP `env` blocks and anything unknown.
pub const CODEX_KEYS: &[&str] = &[
    "model",
    "model_provider",
    "model_reasoning_effort",
    "model_reasoning_summary",
    "model_verbosity",
    "model_context_window",
    "model_max_output_tokens",
    "model_auto_compact_token_limit",
    "approval_policy",
    "sandbox_mode",
    "notify",
    "hide_agent_reasoning",
    "show_raw_agent_reasoning",
    "disable_response_storage",
    "preferred_auth_method",
    "file_opener",
    "tui",
    "features",
    "hooks",
    "tools",
    "history",
    "projects",
    "profile",
    "profiles",
    "mcp_servers",
    "model_providers",
];
pub const CODEX_MCP_KEYS: &[&str] = &[
    "command",
    "args",
    "cwd",
    "url",
    "enabled",
    "startup_timeout_sec",
    "startup_timeout_ms",
    "tool_timeout_sec",
    "enabled_tools",
    "disabled_tools",
    "env_vars",
];
pub const CODEX_PROVIDER_KEYS: &[&str] = &[
    "name",
    "base_url",
    "wire_api",
    "env_key",
    "env_key_instructions",
    "requires_openai_auth",
    "request_max_retries",
    "stream_max_retries",
    "stream_idle_timeout_ms",
];

fn filter_table(t: &toml::Table, keys: &[&str]) -> toml::Table {
    t.iter()
        .filter(|(k, _)| keys.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn sanitize_codex_table(t: &toml::Table, nested: bool) -> toml::Table {
    let mut out = toml::Table::new();
    for (k, v) in t {
        if !CODEX_KEYS.contains(&k.as_str()) || (nested && k == "profiles") {
            continue;
        }
        let v = match (k.as_str(), v) {
            ("mcp_servers", toml::Value::Table(servers)) => toml::Value::Table(
                servers
                    .iter()
                    .filter_map(|(name, s)| {
                        s.as_table().map(|s| {
                            (
                                name.clone(),
                                toml::Value::Table(filter_table(s, CODEX_MCP_KEYS)),
                            )
                        })
                    })
                    .collect(),
            ),
            ("model_providers", toml::Value::Table(ps)) => toml::Value::Table(
                ps.iter()
                    .filter_map(|(name, p)| {
                        p.as_table().map(|p| {
                            (
                                name.clone(),
                                toml::Value::Table(filter_table(p, CODEX_PROVIDER_KEYS)),
                            )
                        })
                    })
                    .collect(),
            ),
            ("profiles", toml::Value::Table(ps)) => toml::Value::Table(
                ps.iter()
                    .filter_map(|(name, p)| {
                        p.as_table().map(|p| {
                            (
                                name.clone(),
                                toml::Value::Table(sanitize_codex_table(p, true)),
                            )
                        })
                    })
                    .collect(),
            ),
            (_, v) => v.clone(),
        };
        out.insert(k.clone(), v);
    }
    out
}

pub fn sanitize_codex_config(bytes: &[u8]) -> Option<Vec<u8>> {
    let t: toml::Table = std::str::from_utf8(bytes).ok()?.parse().ok()?;
    toml::to_string(&sanitize_codex_table(&t, false))
        .ok()
        .map(String::into_bytes)
}

/// Does a JSON key name a secret (`apiKey`, `api_key`, `token`, `headers`, `env`, …)?
fn secret_key(k: &str) -> bool {
    let n: String = k
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    ["apikey", "token", "secret", "password", "credential"]
        .iter()
        .any(|s| n.contains(s))
        || matches!(n.as_str(), "key" | "headers" | "env" | "authorization")
}

fn scrub_json(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(o) => {
            o.retain(|k, _| !secret_key(k));
            for x in o.values_mut() {
                scrub_json(x);
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(scrub_json),
        _ => {}
    }
}

/// pi `settings.json` / `models.json`: everything except secret-named keys (custom providers
/// carry `apiKey` and `headers` in `models.json`).
pub fn sanitize_pi_json(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    scrub_json(&mut v);
    serde_json::to_vec_pretty(&v).ok()
}

/// Read a Vibeke-stored secret file; refuses files readable by group/others.
fn read_private_file(p: &Path) -> Option<String> {
    let md = std::fs::metadata(p).ok()?;
    if md.permissions().mode() & 0o077 != 0 {
        tracing::warn!(path = %p.display(), "credential file ignored: permissions are not 0600");
        return None;
    }
    let s = std::fs::read_to_string(p).ok()?;
    let s = s.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Project credentials for one harness into the pane's private dir.
pub fn project(h: HarnessAuth, inp: &ProjectionInput) -> std::io::Result<Projection> {
    let mut pr = Projection::default();
    let eph = ensure_home(inp.private_dir, &Path::new("home").join(h.id()))?;
    pr.write.push(eph.clone());
    match h {
        HarnessAuth::Claude => {
            let host = inp
                .claude_dir
                .clone()
                .or_else(|| host_var(inp.host_env, "CLAUDE_CONFIG_DIR").map(PathBuf::from))
                .unwrap_or_else(|| inp.home.join(".claude"));
            let settings = host.join("settings.json");
            if settings.exists() {
                copy_sanitized(&settings, &eph, "settings.json", sanitize_claude_settings)?;
            }
            for name in ["CLAUDE.md", "agents", "commands", "skills", "output-styles"] {
                let src = host.join(name);
                if src.exists() {
                    copy_tree(&src, &eph, name.as_ref())?;
                }
            }
            // Minimal global state: no onboarding, no MCP servers or other projects' data
            // (the host `~/.claude.json` can hold secrets in MCP env blocks).
            let mut state = serde_json::json!({"hasCompletedOnboarding": true});
            if let Some(co) = inp.trust_checkout {
                state["projects"] = serde_json::json!({
                    co.to_string_lossy(): {"hasTrustDialogAccepted": true}
                });
                state["bypassPermissionsModeAccepted"] = serde_json::json!(true);
            }
            write_mode(
                &eph.join(".claude.json"),
                serde_json::to_string_pretty(&state)
                    .unwrap_or_default()
                    .as_bytes(),
                0o600,
            )?;
            pr.env.push((
                "CLAUDE_CONFIG_DIR".into(),
                eph.to_string_lossy().into_owned(),
            ));
            // Token: env from the host, else Vibeke's stored setup-token, else the Linux
            // credentials file (macOS keeps it in the Keychain, which is never exposed).
            if let Some(t) = host_var(inp.host_env, "CLAUDE_CODE_OAUTH_TOKEN") {
                pr.secret_env("CLAUDE_CODE_OAUTH_TOKEN", t.to_string());
            } else if let Some(t) =
                read_private_file(&inp.vibeke_credentials.join("claude-oauth-token"))
            {
                pr.secret_env("CLAUDE_CODE_OAUTH_TOKEN", t);
            }
            if let Some(k) = host_var(inp.host_env, "ANTHROPIC_API_KEY") {
                pr.secret_env("ANTHROPIC_API_KEY", k.to_string());
            }
            let cred = host.join(".credentials.json");
            if cred.is_file() {
                let bytes = std::fs::read(&cred)?;
                if let Ok(s) = std::str::from_utf8(&bytes) {
                    pr.secrets.push(s.trim().to_string());
                }
                let dst = eph.join(".credentials.json");
                write_mode(&dst, &bytes, 0o400)?;
                pr.read_only_files.push(dst);
                pr.names.push("file:.claude/.credentials.json".into());
            }
        }
        HarnessAuth::Codex => {
            let host = inp
                .codex_dir
                .clone()
                .or_else(|| host_var(inp.host_env, "CODEX_HOME").map(PathBuf::from))
                .unwrap_or_else(|| inp.home.join(".codex"));
            let config = host.join("config.toml");
            if config.exists() {
                copy_sanitized(&config, &eph, "config.toml", sanitize_codex_config)?;
            }
            for name in ["hooks.json", "AGENTS.md", "prompts"] {
                let src = host.join(name);
                if src.exists() {
                    copy_tree(&src, &eph, name.as_ref())?;
                }
            }
            let auth = host.join("auth.json");
            if auth.is_file() {
                let bytes = std::fs::read(&auth)?;
                if let Ok(s) = std::str::from_utf8(&bytes) {
                    pr.secrets.push(s.trim().to_string());
                }
                let dst = eph.join("auth.json");
                write_mode(&dst, &bytes, 0o400)?;
                pr.read_only_files.push(dst);
                pr.names.push("file:.codex/auth.json".into());
            }
            pr.env
                .push(("CODEX_HOME".into(), eph.to_string_lossy().into_owned()));
            if let Some(k) = host_var(inp.host_env, "OPENAI_API_KEY") {
                pr.secret_env("OPENAI_API_KEY", k.to_string());
            }
        }
        HarnessAuth::Pi => {
            let host = inp
                .pi_agent_dir
                .clone()
                .or_else(|| host_var(inp.host_env, "PI_CODING_AGENT_DIR").map(PathBuf::from))
                .unwrap_or_else(|| inp.home.join(".pi/agent"));
            let agent = crate::fsafe::ensure_dir_under(&eph, Path::new("agent"))?;
            for name in ["settings.json", "models.json"] {
                let src = host.join(name);
                if src.exists() {
                    copy_sanitized(&src, &agent, name, sanitize_pi_json)?;
                }
            }
            for name in ["AGENTS.md", "extensions", "prompts", "themes"] {
                let src = host.join(name);
                if src.exists() {
                    copy_tree(&src, &agent, name.as_ref())?;
                }
            }
            let auth = host.join("auth.json");
            if auth.is_file() {
                let bytes = std::fs::read(&auth)?;
                if let Ok(s) = std::str::from_utf8(&bytes) {
                    pr.secrets.push(s.trim().to_string());
                }
                let dst = agent.join("auth.json");
                write_mode(&dst, &bytes, 0o400)?;
                pr.read_only_files.push(dst);
                pr.names.push("file:.pi/agent/auth.json".into());
            }
            pr.env.push((
                "PI_CODING_AGENT_DIR".into(),
                agent.to_string_lossy().into_owned(),
            ));
            for k in PROVIDER_ENV {
                if let Some(v) = host_var(inp.host_env, k) {
                    pr.secret_env(k, v.to_string());
                }
            }
        }
        HarnessAuth::Omp => {
            // omp has no config-dir override: its tree is readable, only sessions/logs are
            // writable (unverified against omp; see spec 13 status).
            let host = inp.home.join(".omp");
            if host.exists() {
                pr.read.push(host.clone());
                for w in ["agent/sessions", "agent/logs"] {
                    let p = host.join(w);
                    if p.exists() {
                        pr.write.push(p);
                    }
                }
            }
            for k in PROVIDER_ENV {
                if let Some(v) = host_var(inp.host_env, k) {
                    pr.secret_env(k, v.to_string());
                }
            }
        }
    }
    Ok(pr)
}

/// Merge several projections (a task with more than one harness).
pub fn merge(parts: Vec<Projection>) -> Projection {
    let mut out = Projection::default();
    for p in parts {
        for (k, v) in p.env {
            out.env.retain(|(x, _)| x != &k);
            out.env.push((k, v));
        }
        out.read.extend(p.read);
        out.write.extend(p.write);
        out.read_only_files.extend(p.read_only_files);
        out.names.extend(p.names);
        out.secrets.extend(p.secrets);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".claude/agents")).unwrap();
        std::fs::write(home.join(".claude/settings.json"), "{\"hooks\":{}}").unwrap();
        std::fs::write(home.join(".claude/agents/a.md"), "agent").unwrap();
        std::fs::write(
            home.join(".claude/.credentials.json"),
            "{\"claudeAiOauth\":{\"accessToken\":\"sk-ant-oat-FAKE-claude-token-123\"}}",
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/config.toml"), "model = \"x\"\n").unwrap();
        std::fs::write(
            home.join(".codex/auth.json"),
            "{\"tokens\":{\"access_token\":\"FAKE-codex-access-token-456\"}}",
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".pi/agent/extensions/vibeke")).unwrap();
        std::fs::write(home.join(".pi/agent/extensions/vibeke/index.js"), "//").unwrap();
        std::fs::create_dir_all(root.join("state/credentials")).unwrap();
        (t, root)
    }

    #[test]
    fn claude_projection() {
        let (_t, root) = setup();
        let home = root.join("home");
        let creds = root.join("state/credentials");
        let tok = creds.join("claude-oauth-token");
        std::fs::write(&tok, "sk-ant-oat-FAKE-stored-token-789\n").unwrap();
        std::fs::set_permissions(&tok, std::fs::Permissions::from_mode(0o600)).unwrap();
        let private = root.join("state/sbx/p1");
        let env = vec![("AWS_SECRET_ACCESS_KEY".to_string(), "nope".to_string())];
        let pr = project(
            HarnessAuth::Claude,
            &ProjectionInput {
                home: &home,
                host_env: &env,
                vibeke_credentials: &creds,
                private_dir: &private,
                trust_checkout: Some(Path::new("/work/co")),
                claude_dir: None,
                codex_dir: None,
                pi_agent_dir: None,
            },
        )
        .unwrap();
        let eph = private.join("home/claude");
        assert!(eph.join("settings.json").is_file());
        assert!(eph.join("agents/a.md").is_file());
        let state: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(eph.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(
            state["projects"]["/work/co"]["hasTrustDialogAccepted"],
            true
        );
        let get = |k: &str| pr.env.iter().find(|(x, _)| x == k).map(|(_, v)| v.clone());
        assert_eq!(
            get("CLAUDE_CODE_OAUTH_TOKEN").as_deref(),
            Some("sk-ant-oat-FAKE-stored-token-789")
        );
        assert_eq!(get("CLAUDE_CONFIG_DIR").unwrap(), eph.to_string_lossy());
        assert!(get("AWS_SECRET_ACCESS_KEY").is_none());
        let cred = eph.join(".credentials.json");
        assert_eq!(
            std::fs::metadata(&cred).unwrap().permissions().mode() & 0o777,
            0o400
        );
        assert!(pr.read_only_files.contains(&cred));
        // Debug/summary never contain values; redact() scrubs them.
        let dbg = format!("{pr:?}");
        assert!(!dbg.contains("FAKE"));
        assert!(pr.summary().iter().all(|s| !s.contains("FAKE")));
        assert_eq!(
            pr.redact("tok=sk-ant-oat-FAKE-stored-token-789"),
            "tok=[redacted]"
        );
    }

    #[test]
    fn embedded_config_secrets_are_not_projected() {
        let (_t, root) = setup();
        let home = root.join("home");
        // Unrelated secrets embedded in the host harness configs (sentinels).
        std::fs::write(
            home.join(".claude/settings.json"),
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"/usr/bin/vibeke hook claude"}]}]},
                "env":{"AWS_SECRET_ACCESS_KEY":"SENTINEL-aws-in-claude-env","GITHUB_TOKEN":"SENTINEL-gh"},
                "apiKeyHelper":"echo SENTINEL-helper",
                "mcpServers":{"x":{"command":"x","env":{"TOKEN":"SENTINEL-mcp-claude"}}},
                "model":"opus"}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "model = \"gpt-5\"\nexperimental_bearer_token = \"SENTINEL-bearer\"\n\n[features]\nhooks = true\n\n[shell_environment_policy]\nset = { GITHUB_TOKEN = \"SENTINEL-shell-env\" }\n\n[mcp_servers.docs]\ncommand = \"docs-mcp\"\nargs = [\"--fast\"]\nenv = { DOCS_TOKEN = \"SENTINEL-mcp-env\" }\nbearer_token = \"SENTINEL-mcp-bearer\"\nhttp_headers = { Authorization = \"Bearer SENTINEL-hdr\" }\n\n[model_providers.corp]\nname = \"Corp\"\nbase_url = \"https://llm.example.invalid\"\nenv_key = \"CORP_KEY\"\nhttp_headers = { \"X-Api-Key\" = \"SENTINEL-provider-hdr\" }\n\n[profiles.fast]\nmodel = \"o4\"\nexperimental_bearer_token = \"SENTINEL-profile-bearer\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".pi/agent")).unwrap();
        std::fs::write(
            home.join(".pi/agent/models.json"),
            r#"{"providers":{"corp":{"baseUrl":"https://llm.example.invalid","apiKey":"SENTINEL-pi-key","headers":{"X":"SENTINEL-pi-hdr"},"models":[{"id":"m"}]}}}"#,
        )
        .unwrap();
        let private = root.join("state/sbx/p9");
        let inp = ProjectionInput {
            home: &home,
            host_env: &[],
            vibeke_credentials: &root.join("state/credentials"),
            private_dir: &private,
            trust_checkout: None,
            claude_dir: None,
            codex_dir: None,
            pi_agent_dir: None,
        };
        for h in [HarnessAuth::Claude, HarnessAuth::Codex, HarnessAuth::Pi] {
            project(h, &inp).unwrap();
        }
        let mut all = String::new();
        let mut stack = vec![private.join("home")];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if !p.ends_with("auth.json") && !p.ends_with(".credentials.json") {
                    all.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
                }
            }
        }
        assert!(!all.contains("SENTINEL"), "secret projected: {all}");
        // What the harnesses need survives.
        let claude: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(private.join("home/claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(claude["model"], "opus");
        assert!(claude["hooks"]["Stop"].is_array());
        let codex: toml::Table = std::fs::read_to_string(private.join("home/codex/config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(codex["model"].as_str(), Some("gpt-5"));
        assert_eq!(codex["features"]["hooks"].as_bool(), Some(true));
        assert_eq!(
            codex["mcp_servers"]["docs"]["command"].as_str(),
            Some("docs-mcp")
        );
        assert_eq!(
            codex["model_providers"]["corp"]["env_key"].as_str(),
            Some("CORP_KEY")
        );
        assert_eq!(codex["profiles"]["fast"]["model"].as_str(), Some("o4"));
        let pi: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(private.join("home/pi/agent/models.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            pi["providers"]["corp"]["baseUrl"],
            "https://llm.example.invalid"
        );
        // Unparseable settings are not copied at all (rather than verbatim).
        std::fs::write(home.join(".claude/settings.json"), "{ not json SENTINEL").unwrap();
        project(HarnessAuth::Claude, &inp).unwrap();
        assert!(
            !std::fs::read_to_string(private.join("home/claude/settings.json"))
                .unwrap_or_default()
                .contains("SENTINEL")
        );
    }

    #[test]
    fn reprojection_refuses_a_symlinked_home() {
        let (_t, root) = setup();
        let home = root.join("home");
        let private = root.join("state/sbx/p8");
        let inp = ProjectionInput {
            home: &home,
            host_env: &[],
            vibeke_credentials: &root.join("state/credentials"),
            private_dir: &private,
            trust_checkout: None,
            claude_dir: None,
            codex_dir: None,
            pi_agent_dir: None,
        };
        project(HarnessAuth::Claude, &inp).unwrap();
        // The box swaps its projected home for a link to the real home dir.
        let eph = private.join("home/claude");
        std::fs::remove_dir_all(&eph).unwrap();
        let target = root.join("victim");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&target, &eph).unwrap();
        assert!(project(HarnessAuth::Claude, &inp).is_err());
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
        // Nothing was chmod-ed through the link either.
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // A symlinked subdirectory inside the home is refused the same way.
        std::fs::remove_file(&eph).unwrap();
        project(HarnessAuth::Claude, &inp).unwrap();
        std::fs::remove_dir_all(eph.join("agents")).unwrap();
        std::os::unix::fs::symlink(&target, eph.join("agents")).unwrap();
        assert!(project(HarnessAuth::Claude, &inp).is_err());
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }

    #[test]
    fn group_readable_token_file_is_ignored() {
        let (_t, root) = setup();
        let creds = root.join("state/credentials");
        let tok = creds.join("claude-oauth-token");
        std::fs::write(&tok, "sk-ant-oat-FAKE").unwrap();
        std::fs::set_permissions(&tok, std::fs::Permissions::from_mode(0o644)).unwrap();
        let pr = project(
            HarnessAuth::Claude,
            &ProjectionInput {
                home: &root.join("home"),
                host_env: &[],
                vibeke_credentials: &creds,
                private_dir: &root.join("state/sbx/p2"),
                trust_checkout: None,
                claude_dir: None,
                codex_dir: None,
                pi_agent_dir: None,
            },
        )
        .unwrap();
        assert!(!pr.env.iter().any(|(k, _)| k == "CLAUDE_CODE_OAUTH_TOKEN"));
    }

    #[test]
    fn codex_and_pi_projection() {
        let (_t, root) = setup();
        let home = root.join("home");
        let private = root.join("state/sbx/p3");
        let env = vec![
            (
                "OPENROUTER_API_KEY".to_string(),
                "or-FAKE-key-000000".to_string(),
            ),
            ("GITHUB_TOKEN".to_string(), "ghp_nope".to_string()),
        ];
        let inp = ProjectionInput {
            home: &home,
            host_env: &env,
            vibeke_credentials: &root.join("state/credentials"),
            private_dir: &private,
            trust_checkout: None,
            claude_dir: None,
            codex_dir: None,
            pi_agent_dir: None,
        };
        let c = project(HarnessAuth::Codex, &inp).unwrap();
        let eph = private.join("home/codex");
        assert!(eph.join("config.toml").is_file());
        assert!(c.read_only_files.contains(&eph.join("auth.json")));
        assert!(c.names.contains(&"file:.codex/auth.json".to_string()));
        let p = project(HarnessAuth::Pi, &inp).unwrap();
        assert!(
            private
                .join("home/pi/agent/extensions/vibeke/index.js")
                .is_file()
        );
        assert!(p.env.iter().any(|(k, _)| k == "OPENROUTER_API_KEY"));
        assert!(!p.env.iter().any(|(k, _)| k == "GITHUB_TOKEN"));
        let m = merge(vec![c, p]);
        assert!(m.env.iter().any(|(k, _)| k == "CODEX_HOME"));
        assert!(m.env.iter().any(|(k, _)| k == "PI_CODING_AGENT_DIR"));
        assert_eq!(m.secret_count(), 2);
    }
}
