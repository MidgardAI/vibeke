//! OS keychain access for secrets Vibeke keeps outside its files (09 §9.1): the at-rest
//! encryption key of `security.encrypt_state` and assistant credentials named by
//! `credential = { keychain = "…" }` (14 §6).
//!
//! Backends, chosen by `[security] keychain`:
//! - `"os"` (default): the macOS login keychain through `/usr/bin/security`, or the freedesktop
//!   Secret Service (libsecret) through `secret-tool` on Linux. Secrets go over stdin, never on
//!   a command line (other users can read argv on macOS).
//! - `"file:<path>"`: a JSON file (mode 0600, owned by the user) mapping `service/account` to
//!   the secret. Meant for tests and for headless hosts without a keyring; it protects nothing
//!   beyond file permissions, which `vibeke security status` says.
//!
//! Errors carry fixed text: a secret never appears in one.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Default keychain service name for Vibeke's items.
pub const SERVICE: &str = "vibeke";

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
    /// The secret can't be stored by this backend (e.g. contains a newline).
    Invalid(String),
}

impl std::fmt::Display for KeychainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeychainError::Unsupported(m) => write!(f, "keychain unavailable: {m}"),
            KeychainError::Failed(m) => write!(f, "keychain error: {m}"),
            KeychainError::Invalid(m) => write!(f, "keychain: {m}"),
        }
    }
}

impl std::error::Error for KeychainError {}

type R<T> = Result<T, KeychainError>;

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

    pub fn get(&self, service: &str, account: &str) -> R<Option<String>> {
        check_name(service)?;
        check_name(account)?;
        match self {
            Keychain::File(p) => Ok(read_file(p)?.remove(&item(service, account))),
            Keychain::Os => os_get(service, account),
        }
    }

    pub fn set(&self, service: &str, account: &str, secret: &str) -> R<()> {
        check_name(service)?;
        check_name(account)?;
        if secret.is_empty() || secret.chars().any(|c| c.is_control()) {
            return Err(KeychainError::Invalid(
                "the secret must be non-empty and must not contain control characters".into(),
            ));
        }
        match self {
            Keychain::File(p) => {
                let mut m = read_file(p)?;
                m.insert(item(service, account), secret.to_string());
                write_file(p, &m)
            }
            Keychain::Os => os_set(service, account, secret),
        }
    }

    /// Remove an item; `Ok(false)` when it did not exist.
    pub fn delete(&self, service: &str, account: &str) -> R<bool> {
        check_name(service)?;
        check_name(account)?;
        match self {
            Keychain::File(p) => {
                let mut m = read_file(p)?;
                let had = m.remove(&item(service, account)).is_some();
                if had {
                    write_file(p, &m)?;
                }
                Ok(had)
            }
            Keychain::Os => os_delete(service, account),
        }
    }
}

/// Split a credential reference `service/account` (or a bare `account` under [`SERVICE`]).
pub fn parse_ref(r: &str) -> R<(String, String)> {
    let r = r.trim();
    let (s, a) = match r.split_once('/') {
        Some((s, a)) => (s.to_string(), a.to_string()),
        None => (SERVICE.to_string(), r.to_string()),
    };
    check_name(&s)?;
    check_name(&a)?;
    Ok((s, a))
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

fn item(service: &str, account: &str) -> String {
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
    use std::os::unix::fs::MetadataExt;
    let md = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(_) => return Err(KeychainError::Failed("keychain file unreadable".into())),
    };
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    if !md.is_file() || md.uid() != uid || md.mode() & 0o077 != 0 {
        return Err(KeychainError::Failed(
            "keychain file must be a regular file owned by you with mode 0600".into(),
        ));
    }
    let raw =
        std::fs::read(p).map_err(|_| KeychainError::Failed("keychain file unreadable".into()))?;
    if raw.is_empty() {
        return Ok(BTreeMap::new());
    }
    serde_json::from_slice(&raw)
        .map_err(|_| KeychainError::Failed("keychain file is not a JSON object".into()))
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

// ---- OS backends --------------------------------------------------------------------------

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
    let out = child
        .wait_with_output()
        .map_err(|_| KeychainError::Failed("the keychain tool failed".into()))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

#[cfg(target_os = "macos")]
const SECURITY: &str = "/usr/bin/security";

#[cfg(target_os = "macos")]
fn os_get(service: &str, account: &str) -> R<Option<String>> {
    let mut c = Command::new(SECURITY);
    c.args(["find-generic-password", "-s", service, "-a", account, "-w"]);
    match run(c, None)? {
        (0, out) => Ok(Some(out.trim_end_matches('\n').to_string())),
        // errSecItemNotFound
        (44, _) => Ok(None),
        _ => Err(KeychainError::Failed(
            "the macOS Keychain refused the lookup (locked or access denied)".into(),
        )),
    }
}

#[cfg(target_os = "macos")]
fn os_set(service: &str, account: &str, secret: &str) -> R<()> {
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
fn os_delete(service: &str, account: &str) -> R<bool> {
    let mut c = Command::new(SECURITY);
    c.args(["delete-generic-password", "-s", service, "-a", account]);
    match run(c, None)? {
        (0, _) => Ok(true),
        (44, _) => Ok(false),
        _ => Err(KeychainError::Failed(
            "the macOS Keychain refused to delete the item".into(),
        )),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn os_get(service: &str, account: &str) -> R<Option<String>> {
    let mut c = Command::new("secret-tool");
    c.args(["lookup", "service", service, "account", account]);
    match run(c, None)? {
        (0, out) if !out.is_empty() => Ok(Some(out.trim_end_matches('\n').to_string())),
        // secret-tool exits 1 with no output when nothing matches.
        (0 | 1, _) => Ok(None),
        _ => Err(KeychainError::Failed(
            "the Secret Service refused the lookup (locked or no keyring running)".into(),
        )),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn os_set(service: &str, account: &str, secret: &str) -> R<()> {
    let mut c = Command::new("secret-tool");
    let label = format!("Vibeke {service}/{account}");
    c.args([
        "store", "--label", &label, "service", service, "account", account,
    ]);
    match run(c, Some(secret))? {
        (0, _) => Ok(()),
        _ => Err(KeychainError::Failed(
            "the Secret Service refused to store the item".into(),
        )),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn os_delete(service: &str, account: &str) -> R<bool> {
    let existed = os_get(service, account)?.is_some();
    let mut c = Command::new("secret-tool");
    c.args(["clear", "service", service, "account", account]);
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
    fn refs() {
        assert_eq!(
            parse_ref("openai").unwrap(),
            ("vibeke".to_string(), "openai".to_string())
        );
        assert_eq!(
            parse_ref("my-svc/key.1").unwrap(),
            ("my-svc".to_string(), "key.1".to_string())
        );
        assert!(parse_ref("a b").is_err());
        assert!(parse_ref("x/").is_err());
        assert!(parse_ref("").is_err());
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
        // A group-readable file is refused, and the error never carries the content.
        k.set("vibeke", "b", "zzz-SENTINEL").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = k.get("vibeke", "b").unwrap_err();
        assert!(!e.to_string().contains("SENTINEL"));
    }
}
