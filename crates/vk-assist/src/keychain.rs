//! Keychain credential references (14 §6): `credential = { keychain = "vibeke/assistant/primary" }`.
//!
//! Behind `[assistant] keychain_backend` (default `off`, which keeps reporting the reference as
//! unsupported):
//!
//! - `os`: macOS `security find-generic-password -s <item> -w`; elsewhere `secret-tool lookup
//!   service <item>`. The tool is executed directly (no shell), with a ten second limit, and the
//!   secret is read from its stdout only. A missing item or tool is an authentication failure;
//!   there is **no fallback** to another credential source or to ambient provider credentials.
//! - `fake`: tests. `VIBEKE_ASSISTANT_FAKE_KEYCHAIN` names a 0600 JSON object `{item: secret}`
//!   owned by the user. Never used unless selected explicitly.
//!
//! Which store is primary (keychain, env or file) is a product decision that stays with the
//! user (audit section 4, item 21); this module only makes the path available.

use crate::{AssistError, Category, Result};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const FAKE_ENV: &str = "VIBEKE_ASSISTANT_FAKE_KEYCHAIN";
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Off,
    Os,
    Fake,
}

pub fn parse_backend(s: &str) -> Option<Backend> {
    match s {
        "off" | "" => Some(Backend::Off),
        "os" => Some(Backend::Os),
        "fake" => Some(Backend::Fake),
        _ => None,
    }
}

/// Item names are `[A-Za-z0-9._/-]` up to 128 bytes: they reach a child process's argv.
pub fn valid_item(item: &str) -> bool {
    !item.is_empty()
        && item.len() <= 128
        && !item.starts_with('-')
        && item
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
}

fn unsupported(msg: &str) -> AssistError {
    AssistError::new(Category::UnsupportedCapability, msg)
}

fn auth(msg: &str) -> AssistError {
    AssistError::new(Category::AuthenticationFailed, msg)
}

/// Look up `item` in the selected backend.
pub fn lookup(backend: &str, item: &str) -> Result<String> {
    let Some(b) = parse_backend(backend) else {
        return Err(AssistError::new(
            Category::NotConfigured,
            "[assistant] keychain_backend must be off, os or fake",
        ));
    };
    match b {
        Backend::Off => Err(unsupported(
            "keychain credentials are off; set [assistant] keychain_backend = \"os\" to use the system credential store, or use an env or file credential",
        )),
        _ if !valid_item(item) => Err(AssistError::new(
            Category::NotConfigured,
            "the keychain item name has characters outside [A-Za-z0-9._/-]",
        )),
        Backend::Os => os_lookup(item),
        Backend::Fake => fake_lookup(item),
    }
}

fn clean(secret: &str) -> Result<String> {
    let s = secret.lines().next().unwrap_or("").trim().to_string();
    if s.is_empty() {
        return Err(auth("the keychain item is empty"));
    }
    Ok(s)
}

fn fake_lookup(item: &str) -> Result<String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let path = std::env::var_os(FAKE_ENV).ok_or_else(|| {
        auth("the fake keychain is not configured (VIBEKE_ASSISTANT_FAKE_KEYCHAIN)")
    })?;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| auth("the fake keychain file is not readable"))?;
    let meta = f
        .metadata()
        .map_err(|_| auth("the fake keychain file is not readable"))?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o077 != 0 || meta.len() > 65_536 {
        return Err(AssistError::new(
            Category::PermissionDenied,
            "the fake keychain file must be a regular file owned by you with mode 0600",
        ));
    }
    let mut text = String::new();
    (&mut f)
        .take(65_537)
        .read_to_string(&mut text)
        .map_err(|_| auth("the fake keychain file is unreadable"))?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| auth("the fake keychain file is not JSON"))?;
    match v.get(item).and_then(|s| s.as_str()) {
        Some(s) => clean(s),
        None => Err(auth("the keychain item was not found")),
    }
}

fn os_command(item: &str) -> Command {
    if cfg!(target_os = "macos") {
        let mut c = Command::new("/usr/bin/security");
        c.args(["find-generic-password", "-s", item, "-w"]);
        c
    } else {
        let mut c = Command::new("secret-tool");
        c.args(["lookup", "service", item]);
        c
    }
}

fn os_lookup(item: &str) -> Result<String> {
    let mut child = os_command(item)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| auth("the system credential tool is not available"))?;
    let deadline = Instant::now() + TIMEOUT;
    let mut out = child.stdout.take();
    // Read on a thread so a tool that never exits can't block past the deadline.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(o) = out.as_mut() {
            let _ = o.take(65_537).read_to_string(&mut buf);
        }
        let _ = tx.send(buf);
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AssistError::new(
                    Category::Timeout,
                    "the system credential tool did not answer in time",
                ));
            }
        }
    };
    if !status.success() {
        return Err(auth("the keychain item was not found or access was denied"));
    }
    let left = deadline.saturating_duration_since(Instant::now());
    let text = rx
        .recv_timeout(left.max(Duration::from_millis(200)))
        .map_err(|_| auth("the system credential tool returned nothing"))?;
    clean(&text)
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
    }

    #[test]
    fn item_names_are_restricted() {
        assert!(valid_item("vibeke/assistant/primary"));
        for bad in ["", "-s", "a b", "a;rm", "a\nb", &"x".repeat(200)] {
            assert!(!valid_item(bad), "{bad:?}");
        }
        assert_eq!(
            lookup("fake", "-bad").unwrap_err().category,
            Category::NotConfigured
        );
    }

    #[test]
    fn fake_backend_resolves_items_without_fallback() {
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        let f = fake_file(
            d.path(),
            r#"{"vibeke/assistant/primary": "sk-fake-1\n"}"#,
            0o600,
        );
        // SAFETY: serialized by ENV; no other thread reads this variable concurrently.
        unsafe { std::env::set_var(FAKE_ENV, &f) };
        assert_eq!(
            lookup("fake", "vibeke/assistant/primary").unwrap(),
            "sk-fake-1"
        );
        let e = lookup("fake", "vibeke/assistant/other").unwrap_err();
        assert_eq!(e.category, Category::AuthenticationFailed);
        assert!(!e.message.contains("sk-fake"));
        // Wrong mode is refused.
        let f2 = fake_file(d.path(), r#"{"a": "b"}"#, 0o644);
        // SAFETY: as above.
        unsafe { std::env::set_var(FAKE_ENV, &f2) };
        assert_eq!(
            lookup("fake", "a").unwrap_err().category,
            Category::PermissionDenied
        );
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
        let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let d = tempfile::tempdir().unwrap();
        let f = fake_file(
            d.path(),
            r#"{"vibeke/assistant/primary": "sk-fake-2"}"#,
            0o600,
        );
        // SAFETY: serialized by ENV.
        unsafe { std::env::set_var(FAKE_ENV, &f) };
        let c = Connection {
            adapter: Adapter::Anthropic,
            endpoint: None,
            credential: Some(Credential {
                keychain: Some("vibeke/assistant/primary".into()),
                ..Default::default()
            }),
        };
        assert_eq!(
            resolve_credential_with(&c, "fake").unwrap().as_deref(),
            Some("sk-fake-2")
        );
        assert_eq!(
            resolve_credential_with(&c, "off").unwrap_err().category,
            Category::UnsupportedCapability
        );
        // An env fallback never happens: the unresolvable reference stays an error.
        // SAFETY: serialized by ENV.
        unsafe { std::env::remove_var(FAKE_ENV) };
        assert!(resolve_credential_with(&c, "fake").is_err());
    }
}
