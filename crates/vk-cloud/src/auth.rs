//! Provider credentials (spec 17 §5). A provider's credential comes from, in order:
//! 1. the configured reference `[cloud.<provider>] credential = { env | file | keychain }`;
//! 2. the keychain item `vibeke/cloud/<provider>` that `cloud.auth.set` / `vibeke cloud login`
//!    store, through the `[security] keychain` backend;
//! 3. the provider's well-known environment variable ([`AuthMethod::Env`]).
//!
//! Secrets never appear in events, argv, logs or error messages. [`Secret`]'s `Debug` and
//! `Display` print a placeholder.

use serde::{Deserialize, Serialize};
use vk_store::keychain::Keychain;

use crate::config::{CloudConfig, CredentialRef};
use crate::{CloudError, ErrorKind, Provider, Result};

/// A credential value. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Secret {
        Secret(s.into().trim().to_string())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

/// One way to sign in. Clients render these generically: a new provider needs no UI code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthMethod {
    /// Paste a token. `help_url` opens the page where the user creates one.
    PasteToken {
        label: String,
        help_url: String,
        /// Short hint shown under the field ("Starts with the org name").
        hint: String,
    },
    /// Import an existing login on the host (`source` is passed to [`Provider::import`]).
    Import { source: String, label: String },
    /// A well-known environment variable on the host; read-only, shown when set.
    Env { var: String },
}

/// Sign-in state reported by `cloud.providers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthState {
    Missing,
    Ok,
    /// A credential exists but the provider rejected it.
    Invalid,
}

/// Where the credential in use came from (shown, never the value).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Config,
    Keychain,
    Env(String),
}

/// Keychain item for a provider's stored credential.
pub fn item(provider: &str) -> String {
    format!("vibeke/cloud/{provider}")
}

/// The keychain backend from `[security] keychain` (default: the OS keychain).
pub fn keychain(cfg: &vk_config::Config) -> std::result::Result<Keychain, String> {
    match cfg
        .extra
        .get("security")
        .and_then(|s| s.get("keychain"))
        .and_then(|v| v.as_str())
    {
        Some(x) => Keychain::from_setting(x),
        None => Ok(Keychain::Os),
    }
}

fn kc_err(e: impl std::fmt::Display) -> CloudError {
    CloudError::new(ErrorKind::Internal, e.to_string())
}

/// Resolve the credential for `p`. `Ok(None)` when no source has one.
pub fn resolve(
    p: &dyn Provider,
    cfg: &CloudConfig,
    kc: &Keychain,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<(Secret, Source)>> {
    if let Some(r) = cfg.provider(p.id()).credential.as_ref() {
        return r.resolve(kc, env).map(|s| Some((s, Source::Config)));
    }
    if let Some(v) = kc.get_item(&item(p.id())).map_err(kc_err)? {
        let s = Secret::new(v);
        if !s.is_empty() {
            return Ok(Some((s, Source::Keychain)));
        }
    }
    for m in p.auth_methods() {
        if let AuthMethod::Env { var } = m
            && let Some(v) = env(&var).filter(|v| !v.trim().is_empty())
        {
            return Ok(Some((Secret::new(v), Source::Env(var))));
        }
    }
    Ok(None)
}

/// [`resolve`], failing with [`ErrorKind::NeedsAuth`] when nothing is configured.
pub fn require(
    p: &dyn Provider,
    cfg: &CloudConfig,
    kc: &Keychain,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Secret> {
    resolve(p, cfg, kc, env)?
        .map(|(s, _)| s)
        .ok_or_else(|| CloudError::needs_auth(p.id()))
}

/// Store a credential for `provider` in the keychain item.
pub fn store(kc: &Keychain, provider: &str, s: &Secret) -> Result<()> {
    kc.set_item(&item(provider), s.expose()).map_err(kc_err)
}

/// Remove the stored credential; `false` when there was none.
pub fn clear(kc: &Keychain, provider: &str) -> Result<bool> {
    kc.delete_item(&item(provider)).map_err(kc_err)
}

impl CredentialRef {
    /// Exactly one of env, file, keychain. No fallback: an unresolvable reference is an error.
    pub fn resolve(&self, kc: &Keychain, env: &dyn Fn(&str) -> Option<String>) -> Result<Secret> {
        let n = [
            self.env.is_some(),
            self.file.is_some(),
            self.keychain.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count();
        if n != 1 {
            return Err(CloudError::new(
                ErrorKind::InvalidParams,
                "credential must name exactly one of env, file, keychain",
            ));
        }
        let bad = |m: &str| CloudError::new(ErrorKind::NeedsAuth, m.to_string());
        if let Some(var) = &self.env {
            return env(var)
                .map(Secret::new)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad("the configured credential environment variable is not set"));
        }
        if let Some(it) = &self.keychain {
            return kc
                .get_item(it)
                .map_err(kc_err)?
                .map(Secret::new)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| bad("the configured keychain item is empty or missing"));
        }
        let path = crate::config::expand_home(self.file.as_deref().unwrap_or_default());
        read_private_file(&path).map_err(|m| bad(&m))
    }
}

/// Read a 0600 file owned by the user, without following a final symlink. Messages never
/// include the path or its contents.
fn read_private_file(path: &std::path::Path) -> std::result::Result<Secret, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "the configured credential file can't be opened".to_string())?;
    let m = f
        .metadata()
        .map_err(|_| "the configured credential file can't be read".to_string())?;
    if !m.is_file() || m.mode() & 0o077 != 0 || m.uid() != unsafe { libc::getuid() } {
        return Err("the configured credential file must be a regular 0600 file you own".into());
    }
    let mut s = String::new();
    f.take(64 * 1024)
        .read_to_string(&mut s)
        .map_err(|_| "the configured credential file can't be read".to_string())?;
    let s = Secret::new(s);
    if s.is_empty() {
        return Err("the configured credential file is empty".into());
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_print() {
        let s = Secret::new(" tok ");
        assert_eq!(s.expose(), "tok");
        assert_eq!(format!("{s:?} {s}"), "Secret(***) ***");
    }
}
