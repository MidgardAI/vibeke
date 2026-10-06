//! OS keychain access for secrets Vibeke keeps outside its files (09 §9.1): the at-rest
//! encryption key of `security.encrypt_state` and assistant credentials named by
//! `credential = { keychain = "vibeke/assistant/primary" }` (14 §6). This is the one keychain
//! implementation; `vk_assist::keychain` maps its results to assistant error categories.
//!
//! Backends, chosen by `[security] keychain` (`[assistant] keychain_backend` is a deprecated
//! override for assistant credentials only):
//! - `"os"` (default): the macOS login keychain through `/usr/bin/security`, or the freedesktop
//!   Secret Service (libsecret) through `secret-tool` on Linux. Tools run directly (no shell)
//!   with a ten second limit; secrets go over stdin, never on a command line (other users can
//!   read argv on macOS).
//! - `"file:<path>"`: a JSON object file (mode 0600, owned by the user) mapping item names (or
//!   `service/account`) to secrets. Meant for tests and for headless hosts without a keyring;
//!   it protects nothing beyond file permissions, which `vibeke security status` says.
//!
//! Two addressing forms: an **item** (a credential reference: the keychain *service* name, any
//! account; `secret-tool` attribute `service`) and a **service/account pair** (Vibeke's own
//! state keys). Errors carry fixed text: a secret or item value never appears in one.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Default keychain service name for Vibeke's own items.
pub const SERVICE: &str = "vibeke";
/// Account used when Vibeke stores an item (the service name is the item).
const ITEM_ACCOUNT: &str = "vibeke";
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keychain {
    /// macOS Keychain or libsecret.
    Os,
    /// Fake/headless backend: a 0600 JSON file.
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeychainError {
    /// No keychain on this platform or its tool is missing.
    Unsupported(String),
    /// The keychain refused or failed (locked, user denied access, tool error).
    Failed(String),
    /// The secret or name can't be used by this backend (e.g. contains a newline).
    Invalid(String),
    /// The file backend's file has unsafe ownership or mode.
    Permission(String),
    /// The keychain tool did not answer in time.
    Timeout,
}

impl std::fmt::Display for KeychainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeychainError::Unsupported(m) => write!(f, "keychain unavailable: {m}"),
            KeychainError::Failed(m) => write!(f, "keychain error: {m}"),
            KeychainError::Invalid(m) => write!(f, "keychain: {m}"),
            KeychainError::Permission(m) => write!(f, "keychain: {m}"),
            KeychainError::Timeout => {
                write!(f, "keychain: the keychain tool did not answer in time")
            }
        }
    }
}

impl std::error::Error for KeychainError {}

type R<T> = Result<T, KeychainError>;

/// Item names (credential references) are `[A-Za-z0-9._/-]`, up to 128 bytes, not starting
/// with `-`: they reach a child process's argv.
pub fn valid_item(item: &str) -> bool {
    !item.is_empty()
        && item.len() <= 128
        && !item.starts_with('-')
        && item
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
}

impl Keychain {
    /// Parse `[security] keychain`: `os` (or empty) or `file:<path>` (`~/` expanded).
    pub fn from_setting(s: &str) -> Result<Keychain, String> {
        let s = s.trim();
        if s.is_empty() || s == "os" || s == "auto" {
            return Ok(Keychain::Os);
        }
        if let Some(p) = s.strip_prefix("file:") {
            if p.trim().is_empty() {
                return Err("keychain = \"file:<path>\" needs a path".into());
            }
            return Ok(Keychain::File(expand(p.trim())));
        }
        Err("keychain must be \"os\" or \"file:<path>\"".into())
    }

    /// Short description for status output (a path, never a secret).
    pub fn describe(&self) -> String {
        match self {
            Keychain::Os if cfg!(target_os = "macos") => "os (macOS Keychain)".into(),
            Keychain::Os => "os (Secret Service via secret-tool)".into(),
            Keychain::File(p) => format!("file:{}", p.display()),
        }
    }

    /// Setting string that [`Keychain::from_setting`] parses back.
    pub fn setting(&self) -> String {
        match self {
            Keychain::Os => "os".into(),
            Keychain::File(p) => format!("file:{}", p.display()),
        }
    }

    // ---- service/account pairs (Vibeke's state keys) ----

    pub fn get(&self, service: &str, account: &str) -> R<Option<String>> {
        check_name(service)?;
        check_name(account)?;
        match self {
            Keychain::File(p) => Ok(read_file(p)?.remove(&pair(service, account))),
            Keychain::Os => os_get(service, Some(account)),
        }
    }

    pub fn set(&self, service: &str, account: &str, secret: &str) -> R<()> {
        check_name(service)?;
        check_name(account)?;
        check_secret(secret)?;
        match self {
            Keychain::File(p) => file_insert(p, pair(service, account), secret),
            Keychain::Os => os_set(service, account, secret, true),
        }
    }

    /// Remove an item; `Ok(false)` when it did not exist.
    pub fn delete(&self, service: &str, account: &str) -> R<bool> {
        check_name(service)?;
        check_name(account)?;
        match self {
            Keychain::File(p) => file_remove(p, &pair(service, account)),
            Keychain::Os => os_delete(service, Some(account)),
        }
    }

    // ---- items (credential references) ----

    /// Look up a credential reference by item name (keychain service, any account).
    pub fn get_item(&self, item: &str) -> R<Option<String>> {
        check_item(item)?;
        match self {
            Keychain::File(p) => Ok(read_file(p)?.remove(item)),
            Keychain::Os => os_get(item, None),
        }
    }

    pub fn set_item(&self, item: &str, secret: &str) -> R<()> {
        check_item(item)?;
        check_secret(secret)?;
        match self {
            Keychain::File(p) => file_insert(p, item.to_string(), secret),
            Keychain::Os => os_set(item, ITEM_ACCOUNT, secret, false),
        }
    }

    pub fn delete_item(&self, item: &str) -> R<bool> {
        check_item(item)?;
        match self {
            Keychain::File(p) => file_remove(p, item),
            Keychain::Os => os_delete(item, None),
        }
    }
}

fn check_name(n: &str) -> R<()> {
    let ok = !n.is_empty()
        && n.len() <= 128
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@'));
    if ok {
        Ok(())
    } else {
        Err(KeychainError::Invalid(
            "keychain service and account names use letters, digits and -_.:@ (at most 128)".into(),
        ))
    }
}

fn check_item(item: &str) -> R<()> {
    if valid_item(item) {
        Ok(())
    } else {
        Err(KeychainError::Invalid(
            "the keychain item name has characters outside [A-Za-z0-9._/-]".into(),
        ))
    }
}

fn check_secret(secret: &str) -> R<()> {
    if secret.is_empty() || secret.chars().any(|c| c.is_control()) {
        return Err(KeychainError::Invalid(
            "the secret must be non-empty and must not contain control characters".into(),
        ));
    }
    Ok(())
}

fn pair(service: &str, account: &str) -> String {
    format!("{service}/{account}")
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(rest),
        None => PathBuf::from(p),
    }
}

// ---- file backend -------------------------------------------------------------------------

fn read_file(p: &Path) -> R<BTreeMap<String, String>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut f = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(p)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(_) => {
            return Err(KeychainError::Permission(
                "the keychain file is not readable (or is a symbolic link)".into(),
            ));
        }
    };
    let md = f
        .metadata()
        .map_err(|_| KeychainError::Failed("keychain file unreadable".into()))?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    if !md.is_file() || md.uid() != uid || md.mode() & 0o077 != 0 || md.len() > 1 << 20 {
        return Err(KeychainError::Permission(
            "the keychain file must be a regular file owned by you with mode 0600".into(),
        ));
    }
    let mut raw = Vec::new();
    f.read_to_end(&mut raw)
        .map_err(|_| KeychainError::Failed("keychain file unreadable".into()))?;
    if raw.is_empty() {
        return Ok(BTreeMap::new());
    }
    let v: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|_| KeychainError::Failed("keychain file is not a JSON object".into()))?;
    let obj = v
        .as_object()
        .ok_or_else(|| KeychainError::Failed("keychain file is not a JSON object".into()))?;
    Ok(obj
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect())
}

fn write_file(p: &Path, m: &BTreeMap<String, String>) -> R<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let fail = |_| KeychainError::Failed("keychain file not writable".into());
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(fail)?;
    }
    let tmp = p.with_extension(format!("tmp-{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(fail)?;
    f.write_all(&serde_json::to_vec_pretty(m).unwrap_or_default())
        .map_err(fail)?;
    f.sync_all().map_err(fail)?;
    std::fs::rename(&tmp, p).map_err(fail)
}

fn file_insert(p: &Path, key: String, secret: &str) -> R<()> {
    let mut m = read_file(p)?;
    m.insert(key, secret.to_string());
    write_file(p, &m)
}

fn file_remove(p: &Path, key: &str) -> R<bool> {
    let mut m = read_file(p)?;
    let had = m.remove(key).is_some();
    if had {
        write_file(p, &m)?;
    }
    Ok(had)
}

// ---- OS backends --------------------------------------------------------------------------

/// Run a keychain tool with a deadline; returns (exit code, first line of stdout).
fn run(mut cmd: Command, stdin: Option<&str>) -> R<(i32, String)> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            KeychainError::Unsupported("the keychain tool is not installed".into())
        } else {
            KeychainError::Failed("could not start the keychain tool".into())
        }
    })?;
    if let Some(input) = stdin
        && let Some(mut w) = child.stdin.take()
    {
        let _ = w.write_all(input.as_bytes());
    }
    // Read on a thread so a tool that never exits can't block past the deadline.
    let mut out = child.stdout.take();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(o) = out.as_mut() {
            let _ = o.take(65_537).read_to_string(&mut buf);
        }
        let _ = tx.send(buf);
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(KeychainError::Timeout);
            }
        }
    };
    let text = rx
        .recv_timeout(Duration::from_millis(500))
        .unwrap_or_default();
    Ok((
        status.code().unwrap_or(-1),
        text.lines().next().unwrap_or("").to_string(),
    ))
}

#[cfg(target_os = "macos")]
const SECURITY: &str = "/usr/bin/security";

#[cfg(target_os = "macos")]
fn os_get(service: &str, account: Option<&str>) -> R<Option<String>> {
    let mut c = Command::new(SECURITY);
    c.args(["find-generic-password", "-s", service]);
    if let Some(a) = account {
        c.args(["-a", a]);
    }
    c.arg("-w");
    match run(c, None)? {
        (0, out) => Ok(Some(out)),
        // errSecItemNotFound
        (44, _) => Ok(None),
        _ => Err(KeychainError::Failed(
            "the macOS Keychain refused the lookup (locked or access denied)".into(),
        )),
    }
}

#[cfg(target_os = "macos")]
fn os_set(service: &str, account: &str, secret: &str, _pair: bool) -> R<()> {
    // `security -i` reads commands from stdin, keeping the secret off the command line.
    if secret.contains(['"', '\\']) {
        return Err(KeychainError::Invalid(
            "the macOS backend can't store a secret containing quotes or backslashes".into(),
        ));
    }
    let mut c = Command::new(SECURITY);
    c.arg("-i");
    let line =
        format!("add-generic-password -U -s \"{service}\" -a \"{account}\" -w \"{secret}\"\n");
    match run(c, Some(&line))? {
        (0, _) => Ok(()),
        _ => Err(KeychainError::Failed(
            "the macOS Keychain refused to store the item".into(),
        )),
    }
}

#[cfg(target_os = "macos")]
fn os_delete(service: &str, account: Option<&str>) -> R<bool> {
    let mut c = Command::new(SECURITY);
    c.args(["delete-generic-password", "-s", service]);
    if let Some(a) = account {
        c.args(["-a", a]);
    }
    match run(c, None)? {
        (0, _) => Ok(true),
        (44, _) => Ok(false),
        _ => Err(KeychainError::Failed(
            "the macOS Keychain refused to delete the item".into(),
        )),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn attrs<'a>(service: &'a str, account: Option<&'a str>) -> Vec<&'a str> {
    let mut v = vec!["service", service];
    if let Some(a) = account {
        v.extend(["account", a]);
    }
    v
}

#[cfg(all(unix, not(target_os = "macos")))]
fn os_get(service: &str, account: Option<&str>) -> R<Option<String>> {
    let mut c = Command::new("secret-tool");
    c.arg("lookup").args(attrs(service, account));
    match run(c, None)? {
        (0, out) if !out.is_empty() => Ok(Some(out)),
        // secret-tool exits 1 with no output when nothing matches.
        (0 | 1, _) => Ok(None),
        _ => Err(KeychainError::Failed(
            "the Secret Service refused the lookup (locked or no keyring running)".into(),
        )),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn os_set(service: &str, account: &str, secret: &str, pair: bool) -> R<()> {
    let mut c = Command::new("secret-tool");
    let label = format!("Vibeke {service}");
    c.args(["store", "--label", &label])
        .args(attrs(service, pair.then_some(account)));
    match run(c, Some(secret))? {
        (0, _) => Ok(()),
        _ => Err(KeychainError::Failed(
            "the Secret Service refused to store the item".into(),
        )),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn os_delete(service: &str, account: Option<&str>) -> R<bool> {
    let existed = os_get(service, account)?.is_some();
    let mut c = Command::new("secret-tool");
    c.arg("clear").args(attrs(service, account));
    match run(c, None)? {
        (0, _) => Ok(existed),
        _ if !existed => Ok(false),
        _ => Err(KeychainError::Failed(
            "the Secret Service refused to delete the item".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_parse() {
        assert_eq!(Keychain::from_setting("os").unwrap(), Keychain::Os);
        assert_eq!(Keychain::from_setting("").unwrap(), Keychain::Os);
        assert_eq!(
            Keychain::from_setting("file:/x/k.json").unwrap(),
            Keychain::File("/x/k.json".into())
        );
        assert!(Keychain::from_setting("file:").is_err());
        assert!(Keychain::from_setting("gnome").is_err());
        let k = Keychain::File("/x/k.json".into());
        assert_eq!(Keychain::from_setting(&k.setting()).unwrap(), k);
    }

    #[test]
    fn item_names_are_restricted() {
        assert!(valid_item("vibeke/assistant/primary"));
        for bad in ["", "-s", "a b", "a;rm", "a\nb", &"x".repeat(200)] {
            assert!(!valid_item(bad), "{bad:?}");
        }
        let k = Keychain::File("/nonexistent/kc.json".into());
        assert!(matches!(k.get_item("-s"), Err(KeychainError::Invalid(_))));
    }

    #[test]
    fn file_backend_roundtrip_and_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("kc.json");
        let k = Keychain::File(p.clone());
        assert_eq!(k.get("vibeke", "a").unwrap(), None);
        k.set("vibeke", "a", "s3cret-value").unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            k.get("vibeke", "a").unwrap().as_deref(),
            Some("s3cret-value")
        );
        assert_eq!(k.get("other", "a").unwrap(), None);
        assert!(k.set("vibeke", "a", "multi\nline").is_err());
        assert!(k.delete("vibeke", "a").unwrap());
        assert!(!k.delete("vibeke", "a").unwrap());
        // Items live in the same file, keyed by the item name.
        k.set_item("vibeke/assistant/primary", "sk-1").unwrap();
        assert_eq!(
            k.get_item("vibeke/assistant/primary").unwrap().as_deref(),
            Some("sk-1")
        );
        assert!(k.delete_item("vibeke/assistant/primary").unwrap());
        // A group-readable file is refused, and the error never carries the content.
        k.set("vibeke", "b", "zzz-SENTINEL").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = k.get("vibeke", "b").unwrap_err();
        assert!(matches!(e, KeychainError::Permission(_)));
        assert!(!e.to_string().contains("SENTINEL"));
    }
}
