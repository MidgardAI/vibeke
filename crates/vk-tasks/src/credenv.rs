//! Credential-like environment variables (review finding D1.6).

/// Environment variables that commonly hold credentials (cloud keys, forge and registry
/// tokens, the SSH agent). A host check does not get them unless its definition names them.
pub fn is_credential_env(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "SSH_AUTH_SOCK",
        "GITHUB_TOKEN",
        "GH_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "GITLAB_TOKEN",
        "NPM_TOKEN",
        "NODE_AUTH_TOKEN",
        "CARGO_REGISTRY_TOKEN",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "DOCKER_AUTH_CONFIG",
        "HF_TOKEN",
        "VAULT_TOKEN",
    ];
    const SUFFIXES: &[&str] = &[
        "_TOKEN",
        "_SECRET",
        "_SECRET_KEY",
        "_API_KEY",
        "_ACCESS_KEY",
        "_ACCESS_KEY_ID",
        "_PASSWORD",
        "_PASSWD",
        "_CREDENTIALS",
        "_PRIVATE_KEY",
    ];
    let up = name.to_ascii_uppercase();
    if EXACT.contains(&up.as_str()) || SUFFIXES.iter().any(|s| up.ends_with(s)) {
        return true;
    }
    // Every AWS_* except the region settings.
    up.starts_with("AWS_") && !matches!(up.as_str(), "AWS_REGION" | "AWS_DEFAULT_REGION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_common_credentials() {
        for k in [
            "AWS_SECRET_ACCESS_KEY",
            "AWS_PROFILE",
            "GITHUB_TOKEN",
            "SSH_AUTH_SOCK",
            "my_api_key",
            "DB_PASSWORD",
        ] {
            assert!(is_credential_env(k), "{k}");
        }
        for k in [
            "PATH",
            "HOME",
            "AWS_REGION",
            "LANG",
            "TOKENIZERS_PARALLELISM",
        ] {
            assert!(!is_credential_env(k), "{k}");
        }
    }
}
