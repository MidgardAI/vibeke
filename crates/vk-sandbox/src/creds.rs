//! Credential projection (13 §8, 09): each harness gets exactly the model credentials it needs
//! inside the box and nothing else.
//!
//! The harness's config dir is relocated to an **ephemeral home** inside the pane's private dir
//! (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `PI_CODING_AGENT_DIR`), seeded with copies of the
//! non-secret config the user has (settings with Vibeke's hooks, instructions, extensions) plus
//! the credential file, which is made read-only (0400 and a Seatbelt/bind-mount write deny).
//! The box can therefore never modify the host's harness config (hooks there would run on the
//! host later) or the host's credential files.
//!
//! Secret *values* only ever live in the projected files and in the spawn env (which reaches the
//! holder through its 0600 spec file). [`Projection`]'s `Debug` and [`Projection::summary`] name
//! what was projected without values, so events and logs can mention it safely.

use std::fmt;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

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

fn mkdir_private(p: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
}

fn write_mode(p: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    if p.exists() {
        // Read-only copies from an earlier pane: replace atomically-ish.
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
        std::fs::remove_file(p)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(p)?;
    f.write_all(bytes)?;
    drop(f);
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
}

/// Copy a regular file or directory tree (symlinks skipped: they could point anywhere).
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    let md = std::fs::symlink_metadata(src)?;
    if md.file_type().is_symlink() {
        return Ok(());
    }
    if md.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)?.flatten() {
            copy_tree(&e.path(), &dst.join(e.file_name()))?;
        }
        return Ok(());
    }
    if md.is_file() {
        let bytes = std::fs::read(src)?;
        write_mode(dst, &bytes, 0o600)?;
    }
    Ok(())
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
    let eph = inp.private_dir.join("home").join(h.id());
    mkdir_private(&eph)?;
    pr.write.push(eph.clone());
    match h {
        HarnessAuth::Claude => {
            let host = inp
                .claude_dir
                .clone()
                .or_else(|| host_var(inp.host_env, "CLAUDE_CONFIG_DIR").map(PathBuf::from))
                .unwrap_or_else(|| inp.home.join(".claude"));
            for name in [
                "settings.json",
                "CLAUDE.md",
                "agents",
                "commands",
                "skills",
                "output-styles",
            ] {
                let src = host.join(name);
                if src.exists() {
                    copy_tree(&src, &eph.join(name))?;
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
            for name in ["config.toml", "hooks.json", "AGENTS.md", "prompts"] {
                let src = host.join(name);
                if src.exists() {
                    copy_tree(&src, &eph.join(name))?;
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
            let agent = eph.join("agent");
            mkdir_private(&agent)?;
            for name in [
                "settings.json",
                "models.json",
                "AGENTS.md",
                "extensions",
                "prompts",
                "themes",
            ] {
                let src = host.join(name);
                if src.exists() {
                    copy_tree(&src, &agent.join(name))?;
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
