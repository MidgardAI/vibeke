//! Keychain credential references (14 §6): `credential = { keychain = "vibeke/assistant/primary" }`.
//!
//! The keychain itself is `vk_store::keychain` (shared with state encryption, 09 §9.1); this
//! module picks the backend for assistant credentials and maps results to assistant error
//! categories. The backend string is the effective setting:
//!
//! - `""` or `"os"`: the system credential store (macOS `security`, Linux `secret-tool`). An
//!   empty `[assistant] keychain_backend` inherits `[security] keychain`, which the server fills
//!   in before resolving (so `"file:<path>"` can arrive here too).
//! - `"file:<path>"`: the 0600 JSON file backend.
//! - `"off"`: deprecated override that keeps reporting keychain references as unsupported.
//! - `"fake"`: deprecated alias for tests: the file named by `VIBEKE_ASSISTANT_FAKE_KEYCHAIN`.
//!
//! A missing item or tool is an authentication failure; there is **no fallback** to another
//! credential source or to ambient provider credentials. Which store is primary (keychain, env
//! or file) is a product decision that stays with the user (audit section 4, item 21).

use crate::{AssistError, Category, Result};
use vk_store::keychain::{Keychain, KeychainError};

pub use vk_store::keychain::valid_item;

pub const FAKE_ENV: &str = "VIBEKE_ASSISTANT_FAKE_KEYCHAIN";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    Off,
    Os,
    /// Deprecated test alias: the file named by [`FAKE_ENV`].
    Fake,
    File(std::path::PathBuf),
}

/// Parse an effective backend string (`None` when invalid).
pub fn parse_backend(s: &str) -> Option<Backend> {
    match s.trim() {
        "off" => Some(Backend::Off),
        "" | "os" | "auto" => Some(Backend::Os),
        "fake" => Some(Backend::Fake),
        other => match Keychain::from_setting(other) {
            Ok(Keychain::File(p)) => Some(Backend::File(p)),
            Ok(Keychain::Os) => Some(Backend::Os),
            Err(_) => None,
        },
    }
}

fn auth(msg: &str) -> AssistError {
    AssistError::new(Category::AuthenticationFailed, msg)
}

/// Look up `item` in the selected backend.
pub fn lookup(backend: &str, item: &str) -> Result<String> {
    let Some(b) = parse_backend(backend) else {
        return Err(AssistError::new(
            Category::NotConfigured,
            "the keychain backend must be os or file:<path> ([security] keychain), or off/fake in [assistant] keychain_backend",
        ));
    };
    let kc = match b {
        Backend::Off => {
            return Err(AssistError::new(
                Category::UnsupportedCapability,
                "keychain credentials are off ([assistant] keychain_backend = \"off\"); remove that override to use [security] keychain, or use an env or file credential",
            ));
        }
        Backend::Os => Keychain::Os,
        Backend::File(p) => Keychain::File(p),
        Backend::Fake => match std::env::var_os(FAKE_ENV) {
            Some(p) => Keychain::File(p.into()),
            None => {
                return Err(auth(
                    "the fake keychain is not configured (VIBEKE_ASSISTANT_FAKE_KEYCHAIN)",
                ));
            }
        },
    };
    if !valid_item(item) {
        return Err(AssistError::new(
            Category::NotConfigured,
            "the keychain item name has characters outside [A-Za-z0-9._/-]",
        ));
    }
    match kc.get_item(item) {
        Ok(Some(s)) => {
            let s = s.lines().next().unwrap_or("").trim().to_string();
            if s.is_empty() {
                Err(auth("the keychain item is empty"))
            } else {
                Ok(s)
            }
        }
        Ok(None) => Err(auth("the keychain item was not found")),
        Err(KeychainError::Unsupported(_)) => {
            Err(auth("the system credential tool is not available"))
        }
        Err(KeychainError::Permission(_)) => Err(AssistError::new(
            Category::PermissionDenied,
            "the keychain file must be a regular file owned by you with mode 0600",
        )),
        Err(KeychainError::Timeout) => Err(AssistError::new(
            Category::Timeout,
            "the system credential tool did not answer in time",
        )),
        Err(KeychainError::Invalid(_)) => Err(AssistError::new(
            Category::NotConfigured,
            "the keychain item name has characters outside [A-Za-z0-9._/-]",
        )),
        Err(KeychainError::Failed(_)) => {
            Err(auth("the keychain item was not found or access was denied"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The fake backend reads a process-wide environment variable.
    static ENV: Mutex<()> = Mutex::new(());

    fn fake_file(dir: &std::path::Path, json: &str, mode: u32) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("kc.json");
        std::fs::write(&p, json).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        p
    }

    #[test]
    fn off_is_unsupported_and_names_the_switch() {
        let e = lookup("off", "vibeke/assistant/primary").unwrap_err();
        assert_eq!(e.category, Category::UnsupportedCapability);
        assert!(e.message.contains("keychain_backend"));
        assert_eq!(
            lookup("bogus", "x").unwrap_err().category,
            Category::NotConfigured
        );
        assert_eq!(parse_backend(""), Some(Backend::Os));
        assert_eq!(
            parse_backend("file:/x/kc.json"),
            Some(Backend::File("/x/kc.json".into()))
        );
    }

    #[test]
    fn item_names_are_restricted() {
        assert!(valid_item("vibeke/assistant/primary"));
        for bad in ["", "-s", "a b", "a;rm", "a\nb", &"x".repeat(200)] {
            assert!(!valid_item(bad), "{bad:?}");
        }
        assert_eq!(
            lookup("file:/nonexistent/kc.json", "-bad")
                .unwrap_err()
                .category,
            Category::NotConfigured
        );
    }

    #[test]
    fn file_backend_resolves_items_without_fallback() {
        let d = tempfile::tempdir().unwrap();
        let f = fake_file(
            d.path(),
            r#"{"vibeke/assistant/primary": "sk-fake-1\n"}"#,
            0o600,
        );
        let b = format!("file:{}", f.display());
        assert_eq!(lookup(&b, "vibeke/assistant/primary").unwrap(), "sk-fake-1");
        let e = lookup(&b, "vibeke/assistant/other").unwrap_err();
        assert_eq!(e.category, Category::AuthenticationFailed);
        assert!(!e.message.contains("sk-fake"));
        // Wrong mode is refused.
        let f2 = fake_file(d.path(), r#"{"a": "b"}"#, 0o644);
        assert_eq!(
            lookup(&format!("file:{}", f2.display()), "a")
                .unwrap_err()
                .category,
            Category::PermissionDenied
        );
    }

    #[test]
    fn fake_alias_reads_the_env_file() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        let f = fake_file(d.path(), r#"{"a": "sk-env"}"#, 0o600);
        // SAFETY: serialized by ENV; no other thread reads this variable concurrently.
        unsafe { std::env::set_var(FAKE_ENV, &f) };
        assert_eq!(lookup("fake", "a").unwrap(), "sk-env");
        // SAFETY: as above.
        unsafe { std::env::remove_var(FAKE_ENV) };
        assert_eq!(
            lookup("fake", "a").unwrap_err().category,
            Category::AuthenticationFailed
        );
    }

    #[test]
    fn credential_resolution_uses_the_selected_backend() {
        use crate::config::{Adapter, Connection, Credential, resolve_credential_with};
        let d = tempfile::tempdir().unwrap();
        let f = fake_file(
            d.path(),
            r#"{"vibeke/assistant/primary": "sk-fake-2"}"#,
            0o600,
        );
        let b = format!("file:{}", f.display());
        let c = Connection {
            adapter: Adapter::Anthropic,
            endpoint: None,
            credential: Some(Credential {
                keychain: Some("vibeke/assistant/primary".into()),
                ..Default::default()
            }),
        };
        assert_eq!(
            resolve_credential_with(&c, &b).unwrap().as_deref(),
            Some("sk-fake-2")
        );
        assert_eq!(
            resolve_credential_with(&c, "off").unwrap_err().category,
            Category::UnsupportedCapability
        );
        // An env fallback never happens: the unresolvable reference stays an error.
        std::fs::remove_file(&f).unwrap();
        assert!(resolve_credential_with(&c, &b).is_err());
    }
}
