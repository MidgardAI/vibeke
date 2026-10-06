//! Signed manifest update channel, client side (04 §13).
//!
//! `vibeke integration update [--url U]` is the only trigger: there is no background polling
//! and nothing enables it automatically (`VIBEKE_MANIFEST_CHANNEL=0` refuses even explicit
//! updates). Flow: fetch `index.json` + `index.json.minisig` → verify the signature against keys
//! compiled into the binary → `serial` must strictly increase (no rollback) → fetch each listed
//! manifest whose id is a built-in harness and whose `min_vibeke..max_vibeke` covers this build
//! → sha256 must match the index → stage, then swap into `<state>/manifests/remote/` atomically.
//! The loader (`vk_agents::manifest::load`) strips process-spawning fields and unattested
//! capability rows from remote manifests.
//!
//! Signature verification follows `vk-remote::bootstrap::verify_signature`: no Ed25519
//! implementation is in the workspace and no release key exists yet, so verification always
//! fails and an update is refused unless the user opts in to unsigned development indexes with
//! `VIBEKE_ALLOW_UNSIGNED_MANIFESTS=1` (sha256 and serial checks still apply).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;
use std::path::{Path, PathBuf};

pub const DEFAULT_URL: &str = "https://manifests.vibeke.dev/v1/stable/index.json";

/// Minisign public keys trusted to sign the index (two slots for rotation). None exist yet.
pub const TRUSTED_KEYS: &[&str] = &[];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelError {
    Disabled,
    NoTrustedKeys,
    Rollback { have: u64, got: u64 },
    Checksum { id: String },
    Fetch(String),
    Invalid(String),
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelError::Disabled => {
                write!(f, "manifest channel disabled (VIBEKE_MANIFEST_CHANNEL=0)")
            }
            ChannelError::NoTrustedKeys => write!(
                f,
                "this build embeds no manifest signing keys (none exist yet); refusing the unsigned index \
                 (VIBEKE_ALLOW_UNSIGNED_MANIFESTS=1 accepts it for development)"
            ),
            ChannelError::Rollback { have, got } => {
                write!(
                    f,
                    "index serial {got} is not newer than the cached serial {have} (rollback refused)"
                )
            }
            ChannelError::Checksum { id } => write!(f, "{id}: sha256 does not match the index"),
            ChannelError::Fetch(e) => write!(f, "fetch failed: {e}"),
            ChannelError::Invalid(e) => write!(f, "invalid index: {e}"),
        }
    }
}

impl std::error::Error for ChannelError {}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IndexEntry {
    pub id: String,
    pub version: String,
    pub sha256: String,
    pub url: String,
    #[serde(default)]
    pub min_vibeke: String,
    #[serde(default)]
    pub max_vibeke: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Index {
    pub serial: u64,
    #[serde(default)]
    pub created_at: String,
    pub manifests: Vec<IndexEntry>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct State {
    pub serial: u64,
    pub created_at: String,
    /// `signature` or `unsigned-dev`.
    pub verified: String,
    pub applied: Vec<String>,
    pub url: String,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub serial: u64,
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
    pub unsigned: bool,
    pub warnings: Vec<String>,
}

pub fn root() -> PathBuf {
    crate::paths::state_root().join("manifests")
}

fn state_path(root: &Path) -> PathBuf {
    root.join("remote-state.json")
}

pub fn read_state(root: &Path) -> Option<State> {
    serde_json::from_str(&std::fs::read_to_string(state_path(root)).ok()?).ok()
}

/// The verified cache for the loader: `(dir with <id>.toml, serial)`.
pub fn cached_dir() -> Option<(PathBuf, u64)> {
    let r = root();
    let st = read_state(&r)?;
    let dir = r.join("remote");
    dir.is_dir().then_some((dir, st.serial))
}

/// Same contract as `vk-remote::bootstrap::verify_signature`: refuses while no key exists.
pub fn verify_signature(_index: &[u8], _sig: Option<&[u8]>) -> Result<(), ChannelError> {
    if TRUSTED_KEYS.is_empty() {
        return Err(ChannelError::NoTrustedKeys);
    }
    // Unreachable until release keys and an Ed25519 verifier land.
    Err(ChannelError::NoTrustedKeys)
}

pub fn allow_unsigned_env() -> bool {
    std::env::var("VIBEKE_ALLOW_UNSIGNED_MANIFESTS").is_ok_and(|v| v == "1")
}

fn fetch(url: &str) -> Result<Vec<u8>, ChannelError> {
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).map_err(|e| ChannelError::Fetch(format!("{url}: {e}")));
    }
    if !url.starts_with("https://") {
        return Err(ChannelError::Fetch(format!(
            "{url}: only https:// and file:// are allowed"
        )));
    }
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "--max-time", "20", url])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| ChannelError::Fetch(format!("curl: {e}")))?;
    if !out.status.success() {
        return Err(ChannelError::Fetch(format!(
            "{url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

fn join_url(index: &str, rel: &str) -> String {
    if rel.contains("://") {
        return rel.to_string();
    }
    match index.rfind('/') {
        Some(i) => format!("{}/{rel}", &index[..i]),
        None => rel.to_string(),
    }
}

fn version_ok(e: &IndexEntry) -> bool {
    let me = vk_agents::manifest::version_triple(vk_proto::VERSION).0;
    let lo =
        (!e.min_vibeke.is_empty()).then(|| vk_agents::manifest::version_triple(&e.min_vibeke).0);
    let hi =
        (!e.max_vibeke.is_empty()).then(|| vk_agents::manifest::version_triple(&e.max_vibeke).0);
    lo.is_none_or(|l| me >= l) && hi.is_none_or(|h| me <= h)
}

/// Fetch, verify and install the remote manifests into `root` (`<state>/manifests` normally).
pub fn update(url: &str, root: &Path) -> Result<Report, ChannelError> {
    if std::env::var("VIBEKE_MANIFEST_CHANNEL").is_ok_and(|v| v == "0") {
        return Err(ChannelError::Disabled);
    }
    let index_bytes = fetch(url)?;
    let sig = fetch(&format!("{url}.minisig")).ok();
    let mut report = Report::default();
    match verify_signature(&index_bytes, sig.as_deref()) {
        Ok(()) => {}
        Err(_) if allow_unsigned_env() => {
            report.unsigned = true;
            report.warnings.push(format!(
                "WARNING: VIBEKE_ALLOW_UNSIGNED_MANIFESTS=1: using an UNSIGNED manifest index from {url}"
            ));
        }
        Err(e) => return Err(e),
    }
    let index: Index =
        serde_json::from_slice(&index_bytes).map_err(|e| ChannelError::Invalid(e.to_string()))?;
    let have = read_state(root).map(|s| s.serial).unwrap_or(0);
    if index.serial <= have {
        return Err(ChannelError::Rollback {
            have,
            got: index.serial,
        });
    }
    let builtin: Vec<String> = vk_agents::manifest::BUILTIN
        .iter()
        .filter_map(|(_, t)| {
            vk_agents::manifest::parse_raw(t, vk_agents::manifest::Source::Builtin).ok()
        })
        .map(|r| r.id)
        .collect();
    let staging = root.join(format!("remote.staging-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| ChannelError::Fetch(e.to_string()))?;
    for e in &index.manifests {
        // Unknown harness ids are skipped silently (04 §13).
        if !builtin.contains(&e.id) {
            continue;
        }
        if !version_ok(e) {
            report.skipped.push(format!(
                "{} (needs vibeke {}..{})",
                e.id, e.min_vibeke, e.max_vibeke
            ));
            continue;
        }
        let bytes = fetch(&join_url(url, &e.url))?;
        let got = format!("{:x}", sha2::Sha256::digest(&bytes));
        if !got.eq_ignore_ascii_case(&e.sha256) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(ChannelError::Checksum { id: e.id.clone() });
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| ChannelError::Invalid(format!("{}: not UTF-8", e.id)))?;
        let raw = vk_agents::manifest::parse_raw(
            &text,
            vk_agents::manifest::Source::Remote {
                serial: index.serial,
            },
        )
        .map_err(|err| ChannelError::Invalid(format!("{}: {err:#}", e.id)))?;
        if raw.id != e.id {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(ChannelError::Invalid(format!(
                "{}: file declares id {}",
                e.id, raw.id
            )));
        }
        std::fs::write(staging.join(format!("{}.toml", e.id)), text)
            .map_err(|err| ChannelError::Fetch(err.to_string()))?;
        report.applied.push(format!("{}@{}", e.id, e.version));
    }
    let dest = root.join("remote");
    let old = root.join(format!("remote.old-{}", std::process::id()));
    if dest.exists() {
        std::fs::rename(&dest, &old).map_err(|e| ChannelError::Fetch(e.to_string()))?;
    }
    std::fs::rename(&staging, &dest).map_err(|e| ChannelError::Fetch(e.to_string()))?;
    let _ = std::fs::remove_dir_all(&old);
    let st = State {
        serial: index.serial,
        created_at: index.created_at.clone(),
        verified: if report.unsigned {
            "unsigned-dev"
        } else {
            "signature"
        }
        .into(),
        applied: report.applied.clone(),
        url: url.to_string(),
    };
    let tmp = root.join("remote-state.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&st).unwrap_or_default())
        .and_then(|_| std::fs::rename(&tmp, state_path(root)))
        .map_err(|e| ChannelError::Fetch(e.to_string()))?;
    report.serial = index.serial;
    Ok(report)
}

/// `vibeke integration channel` status as JSON.
pub fn status_json(root: &Path) -> Value {
    match read_state(root) {
        Some(s) => {
            serde_json::json!({"serial": s.serial, "created_at": s.created_at, "verified": s.verified, "applied": s.applied, "url": s.url})
        }
        None => {
            serde_json::json!({"serial": null, "note": "no remote manifests installed (run `vibeke integration update`)"})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV: Mutex<()> = Mutex::new(());

    fn publish(dir: &Path, serial: u64, body: &str, tamper: bool) -> String {
        std::fs::write(dir.join("gemini.toml"), body).unwrap();
        let sha = format!("{:x}", sha2::Sha256::digest(body.as_bytes()));
        let sha = if tamper { "0".repeat(64) } else { sha };
        let index = serde_json::json!({"serial": serial, "created_at": "2026-10-06", "manifests": [
            {"id": "gemini", "version": "2", "sha256": sha, "url": "gemini.toml"},
            {"id": "not-a-harness", "version": "1", "sha256": "x", "url": "nope.toml"}
        ]});
        std::fs::write(dir.join("index.json"), index.to_string()).unwrap();
        format!("file://{}/index.json", dir.display())
    }

    #[test]
    fn refuses_unsigned_then_dev_opt_in_with_serial_and_checksum_rules() {
        let _g = ENV.lock().unwrap();
        let srv = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let body = "id = \"gemini\"\n[[screen.rules]]\nid = \"x\"\nstate = \"working\"\nany = ['busy']\n[launch]\nargv = [\"evil\"]\n";
        let url = publish(srv.path(), 3, body, false);
        // SAFETY: test-only env mutation, serialized by ENV.
        unsafe { std::env::remove_var("VIBEKE_ALLOW_UNSIGNED_MANIFESTS") };
        assert_eq!(
            update(&url, root.path()).unwrap_err(),
            ChannelError::NoTrustedKeys
        );
        assert!(
            read_state(root.path()).is_none(),
            "nothing cached without a signature"
        );
        unsafe { std::env::set_var("VIBEKE_ALLOW_UNSIGNED_MANIFESTS", "1") };
        let r = update(&url, root.path()).unwrap();
        assert!(r.unsigned);
        assert_eq!(r.applied, vec!["gemini@2"]);
        assert_eq!(read_state(root.path()).unwrap().verified, "unsigned-dev");
        // Same serial again: rollback refused.
        assert!(matches!(
            update(&url, root.path()),
            Err(ChannelError::Rollback { have: 3, got: 3 })
        ));
        // Tampered file: checksum refused, previous cache intact.
        let url = publish(srv.path(), 4, body, true);
        assert_eq!(
            update(&url, root.path()).unwrap_err(),
            ChannelError::Checksum {
                id: "gemini".into()
            }
        );
        assert_eq!(read_state(root.path()).unwrap().serial, 3);
        // The loader strips process-spawning fields from the cached remote manifest.
        let set = vk_agents::manifest::load(&vk_agents::manifest::Sources {
            remote: Some((root.path().join("remote"), 3)),
            ..Default::default()
        });
        let g = set.get("gemini").unwrap();
        assert_eq!(g.m.launch.argv, vec!["gemini".to_string()]);
        unsafe { std::env::remove_var("VIBEKE_ALLOW_UNSIGNED_MANIFESTS") };
        assert!(matches!(
            fetch("http://example.com/x"),
            Err(ChannelError::Fetch(_))
        ));
    }
}
