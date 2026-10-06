//! Manifest-declared credential projection (13 §8, harness manifest `[auth]`) for harnesses
//! without built-in rules, and the store for Claude's `setup-token`.
//!
//! A manifest names env variables and home-relative files; only those are projected, never
//! git-push, SSH or cloud credentials (refused even when declared). Files are copied read-only
//! into the harness's ephemeral home under the private dir, keeping their home-relative path;
//! `home_env` (if set) points the harness at that home. Without it, the sandbox level instead
//! allowlists the original files read-only.

use super::{Projection, ProjectionInput, ensure_home, host_var, write_mode};
use std::path::{Component, Path, PathBuf};

/// A harness manifest's `[auth]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeclaredAuth {
    /// Host env variables projected as secrets (by name).
    pub env: Vec<String>,
    /// Home-relative files (`~/.foo/token`) copied read-only into the ephemeral home.
    pub files: Vec<String>,
    /// Env variable the harness reads its config home from (set to the ephemeral home).
    pub home_env: Option<String>,
}

/// Env names a manifest can never project (13 §8): push, SSH, cloud and registry credentials.
pub fn env_refused(k: &str) -> bool {
    const EXACT: &[&str] = &[
        "SSH_AUTH_SOCK",
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITLAB_TOKEN",
        "NPM_TOKEN",
        "NODE_AUTH_TOKEN",
        "CARGO_REGISTRY_TOKEN",
        "PYPI_TOKEN",
        "TWINE_PASSWORD",
        "DOCKER_AUTH_CONFIG",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "KUBECONFIG",
        "HOME",
        "PATH",
        "VIBEKE_SOCKET",
        "VIBEKE_TOKEN",
    ];
    const PREFIX: &[&str] = &[
        "AWS_",
        "AZURE_",
        "ARM_",
        "GCP_",
        "GCLOUD_",
        "CLOUDSDK_",
        "DIGITALOCEAN_",
        "HEROKU_",
        "VERCEL_",
        "NETLIFY_",
        "CLOUDFLARE_",
        "VIBEKE_",
    ];
    EXACT.contains(&k) || PREFIX.iter().any(|p| k.starts_with(p))
}

/// Home-relative locations a manifest can never project.
const FILES_REFUSED: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".gnupg",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".config/gcloud",
    ".config/gh",
    ".config/vibeke",
    ".local/state/vibeke",
];

/// `~/rel` → `rel` when it is a plain relative path inside the home (no `..`, not absolute).
fn home_rel(p: &str) -> Option<PathBuf> {
    let rel = p.strip_prefix("~/")?;
    let rel = Path::new(rel);
    let ok = rel.components().all(|c| matches!(c, Component::Normal(_)))
        && rel.components().next().is_some();
    ok.then(|| rel.to_path_buf())
}

pub fn file_refused(rel: &Path) -> bool {
    FILES_REFUSED.iter().any(|r| rel.starts_with(r))
}

fn valid_env_name(k: &str) -> bool {
    !k.is_empty()
        && k.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !k.starts_with(|c: char| c.is_ascii_digit())
}

/// Project what a manifest declares for harness `id` into `inp.private_dir`. Refused names
/// and paths are skipped and reported in the returned warnings.
pub fn project_declared(
    id: &str,
    auth: &DeclaredAuth,
    inp: &ProjectionInput,
) -> std::io::Result<(Projection, Vec<String>)> {
    let mut pr = Projection::default();
    let mut warnings = Vec::new();
    let dir: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let eph = ensure_home(inp.private_dir, &Path::new("home").join(&dir))?;
    pr.write.push(eph.clone());
    for k in &auth.env {
        if !valid_env_name(k) || env_refused(k) {
            warnings.push(format!("[auth] env {k} is never projected"));
            continue;
        }
        if let Some(v) = host_var(inp.host_env, k) {
            pr.secret_env(k, v.to_string());
        }
    }
    for f in &auth.files {
        let Some(rel) = home_rel(f) else {
            warnings.push(format!("[auth] file {f} must be a ~/ path inside the home"));
            continue;
        };
        if file_refused(&rel) {
            warnings.push(format!("[auth] file {f} is never projected"));
            continue;
        }
        let src = inp.home.join(&rel);
        let Ok(md) = std::fs::symlink_metadata(&src) else {
            continue;
        };
        if !md.is_file() {
            // Links could point anywhere; directories are not credentials.
            warnings.push(format!("[auth] file {f} is not a regular file; skipped"));
            continue;
        }
        let bytes = std::fs::read(&src)?;
        if let Ok(s) = std::str::from_utf8(&bytes) {
            let s = s.trim();
            if s.len() >= 8 {
                pr.secrets.push(s.to_string());
            }
        }
        let parent = rel.parent().unwrap_or(Path::new(""));
        let dst_dir = if parent.as_os_str().is_empty() {
            eph.clone()
        } else {
            crate::fsafe::ensure_dir_under(&eph, parent)?
        };
        let dst = dst_dir.join(rel.file_name().unwrap_or_default());
        write_mode(&dst, &bytes, 0o400)?;
        pr.read_only_files.push(dst);
        pr.names.push(format!("file:{}", rel.display()));
        if auth.home_env.is_none() {
            // The harness reads the original path: readable (only that file) in the sandbox.
            pr.read.push(src);
        }
    }
    if let Some(h) = auth.home_env.as_deref() {
        if valid_env_name(h) && !env_refused(h) {
            pr.env.retain(|(x, _)| x != h);
            pr.env
                .push((h.to_string(), eph.to_string_lossy().into_owned()));
        } else {
            warnings.push(format!("[auth] home_env {h} is not allowed"));
        }
    }
    Ok((pr, warnings))
}

/// Store the long-lived token from `claude setup-token` (13 §8) as
/// `<credentials>/claude-oauth-token`, 0600, never following a symlink. Returns the path.
pub fn store_claude_setup_token(credentials: &Path, token: &str) -> std::io::Result<PathBuf> {
    let token = token.trim();
    if token.len() < 16 || token.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "that does not look like a setup token (expected one line, no spaces)",
        ));
    }
    if !credentials.exists() {
        std::fs::create_dir_all(credentials)?;
    }
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(credentials, std::fs::Permissions::from_mode(0o700))?;
    }
    let p = credentials.join("claude-oauth-token");
    if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
        std::fs::remove_file(&p)?;
    }
    write_mode(&p, format!("{token}\n").as_bytes(), 0o600)?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(
        home: &'a Path,
        env: &'a [(String, String)],
        private: &'a Path,
    ) -> ProjectionInput<'a> {
        ProjectionInput {
            home,
            host_env: env,
            vibeke_credentials: private,
            private_dir: private,
            trust_checkout: None,
            claude_dir: None,
            codex_dir: None,
            pi_agent_dir: None,
        }
    }

    #[test]
    fn declared_env_and_files_only_what_is_safe() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".foo")).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".foo/token"), "FOO-SECRET-TOKEN-1234").unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "KEY").unwrap();
        std::os::unix::fs::symlink(home.join(".ssh/id_ed25519"), home.join(".foo/link")).unwrap();
        let env = vec![
            ("FOO_API_KEY".to_string(), "foo-key-value-123".to_string()),
            ("AWS_SECRET_ACCESS_KEY".to_string(), "aws".to_string()),
            ("GITHUB_TOKEN".to_string(), "gh".to_string()),
        ];
        let private = root.join("private");
        let auth = DeclaredAuth {
            env: vec![
                "FOO_API_KEY".into(),
                "AWS_SECRET_ACCESS_KEY".into(),
                "GITHUB_TOKEN".into(),
                "bad name".into(),
            ],
            files: vec![
                "~/.foo/token".into(),
                "~/.ssh/id_ed25519".into(),
                "~/../etc/passwd".into(),
                "/etc/hosts".into(),
                "~/.foo/link".into(),
            ],
            home_env: None,
        };
        let (pr, w) = project_declared("repo:foo", &auth, &input(&home, &env, &private)).unwrap();
        assert_eq!(pr.env.len(), 1);
        assert_eq!(pr.env[0].0, "FOO_API_KEY");
        assert!(pr.names.contains(&"env:FOO_API_KEY".to_string()));
        assert!(pr.names.contains(&"file:.foo/token".to_string()));
        assert_eq!(pr.read_only_files.len(), 1);
        let copied = &pr.read_only_files[0];
        assert!(copied.starts_with(private.join("home/repo_foo")));
        assert_eq!(
            std::fs::read_to_string(copied).unwrap(),
            "FOO-SECRET-TOKEN-1234"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(copied).unwrap().permissions().mode() & 0o777,
            0o400
        );
        // Without home_env the original file is what the sandbox allowlists.
        assert_eq!(pr.read, vec![home.join(".foo/token")]);
        assert_eq!(pr.redact("x FOO-SECRET-TOKEN-1234 y"), "x [redacted] y");
        assert!(w.iter().any(|x| x.contains("AWS_SECRET_ACCESS_KEY")));
        assert!(w.iter().any(|x| x.contains("GITHUB_TOKEN")));
        assert!(w.iter().any(|x| x.contains(".ssh")));
        assert!(w.iter().any(|x| x.contains("../etc/passwd")));
        assert!(w.iter().any(|x| x.contains("/etc/hosts")));
        assert!(w.iter().any(|x| x.contains("link")));
        // home_env points the harness at the ephemeral home instead.
        let auth = DeclaredAuth {
            files: vec!["~/.foo/token".into()],
            home_env: Some("FOO_HOME".into()),
            ..Default::default()
        };
        let (pr, _) =
            project_declared("foo", &auth, &input(&home, &env, &root.join("p2"))).unwrap();
        assert!(pr.read.is_empty());
        assert!(
            pr.env
                .iter()
                .any(|(k, v)| k == "FOO_HOME" && v.ends_with("home/foo"))
        );
    }

    #[test]
    fn setup_token_store() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("credentials");
        assert!(store_claude_setup_token(&dir, "short").is_err());
        assert!(store_claude_setup_token(&dir, "has a space in it 12345").is_err());
        let p = store_claude_setup_token(&dir, "  sk-ant-oat01-FAKE-setup-token-xyz \n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "sk-ant-oat01-FAKE-setup-token-xyz\n"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // The projection reads it back.
        assert_eq!(
            super::super::read_private_file(&p).as_deref(),
            Some("sk-ant-oat01-FAKE-setup-token-xyz")
        );
        // A planted symlink is replaced, not followed.
        let victim = t.path().join("victim");
        std::fs::write(&victim, "keep").unwrap();
        std::fs::remove_file(&p).unwrap();
        std::os::unix::fs::symlink(&victim, &p).unwrap();
        store_claude_setup_token(&dir, "sk-ant-oat01-FAKE-setup-token-2").unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
    }
}
