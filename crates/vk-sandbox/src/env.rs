//! Environment of a contained process tree (13 §8): an allowlist instead of the host env, so
//! cloud keys, `GITHUB_TOKEN`, `SSH_AUTH_SOCK` and friends never cross the boundary. Projected
//! credentials and proxy settings are added by the caller.

use std::path::Path;

/// Host variables passed through unchanged.
pub const PASS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TZ",
    "EDITOR",
    "VISUAL",
    "PAGER",
    "TERM",
    "COLORTERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "COLUMNS",
    "LINES",
    "MISE_SHELL",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "GOPATH",
    "NVM_DIR",
    "VOLTA_HOME",
    "PNPM_HOME",
    "BUN_INSTALL",
    "DENO_INSTALL",
    "PYENV_ROOT",
    "JAVA_HOME",
    "NO_COLOR",
    "FORCE_COLOR",
    "CLICOLOR",
];

/// Prefixes passed through (`LC_*` locale, Vibeke's own pane identity).
pub const PASS_PREFIX: &[&str] = &["LC_", "VIBEKE"];

/// Keep only allowlisted variables from `env`.
pub fn scrub(env: &[(String, String)]) -> Vec<(String, String)> {
    env.iter()
        .filter(|(k, _)| PASS.contains(&k.as_str()) || PASS_PREFIX.iter().any(|p| k.starts_with(p)))
        .cloned()
        .collect()
}

pub fn set(env: &mut Vec<(String, String)>, k: &str, v: impl Into<String>) {
    env.retain(|(x, _)| x != k);
    env.push((k.to_string(), v.into()));
}

/// Proxy variables for every common client spelling.
pub fn proxy_env(env: &mut Vec<(String, String)>, port: u16) {
    let url = format!("http://127.0.0.1:{port}");
    for k in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "npm_config_proxy",
        "npm_config_https_proxy",
    ] {
        set(env, k, url.clone());
    }
    // Node ≥ 24 honours HTTP(S)_PROXY for fetch only with this flag.
    set(env, "NODE_USE_ENV_PROXY", "1");
    for k in ["NO_PROXY", "no_proxy"] {
        set(env, k, "");
    }
}

/// Private temp and cache dirs under the pane's private dir, plus a zsh dotdir that marks the
/// prompt (the user's rc files are not readable inside: they often export secrets).
pub fn private_dirs(env: &mut Vec<(String, String)>, private: &Path) -> std::io::Result<()> {
    let tmp = private.join("tmp");
    let cache = private.join("cache");
    let zdot = private.join("zdot");
    for d in [&tmp, &cache, &zdot] {
        std::fs::create_dir_all(d)?;
    }
    let zshrc = zdot.join(".zshrc");
    if !zshrc.exists() {
        std::fs::write(
            &zshrc,
            "# Vibeke sandbox shell (spec 13): your ~/.zshrc is not readable in here.\nPROMPT='%F{yellow}[sbx]%f %~ %# '\n",
        )?;
    }
    let s = |p: &Path| p.to_string_lossy().into_owned();
    set(env, "TMPDIR", format!("{}/", s(&tmp)));
    set(env, "XDG_CACHE_HOME", s(&cache));
    set(env, "npm_config_cache", s(&cache.join("npm")));
    set(env, "PIP_CACHE_DIR", s(&cache.join("pip")));
    set(env, "ZDOTDIR", s(&zdot));
    set(env, "VIBEKE_SANDBOX", "1");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_and_proxy() {
        let host = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("AWS_SECRET_ACCESS_KEY".to_string(), "x".to_string()),
            ("SSH_AUTH_SOCK".to_string(), "/tmp/agent".to_string()),
            ("GITHUB_TOKEN".to_string(), "ghp".to_string()),
            ("LC_ALL".to_string(), "C".to_string()),
            ("VIBEKE_PANE_ID".to_string(), "w1:p1".to_string()),
        ];
        let mut e = scrub(&host);
        let keys: Vec<&str> = e.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["PATH", "LC_ALL", "VIBEKE_PANE_ID"]);
        proxy_env(&mut e, 4100);
        assert!(e.contains(&("HTTPS_PROXY".into(), "http://127.0.0.1:4100".into())));
        let t = tempfile::tempdir().unwrap();
        private_dirs(&mut e, t.path()).unwrap();
        assert!(t.path().join("zdot/.zshrc").is_file());
        assert!(e.iter().any(|(k, v)| k == "TMPDIR" && v.ends_with("/tmp/")));
    }
}
