//! Signed manifest update channel, client side (04 §13).
//!
//! Triggers: `vibeke integration update [--url U]`, and the server's poller ([`start_polling`]):
//! every 6 h (first poll 15 minutes after start) unless `[update] manifest_check = false` or
//! `VIBEKE_MANIFEST_CHANNEL=0` (which also refuses explicit updates). A manifest can be frozen
//! with `vibeke integration pin <id>` ([`pin`]): updates leave a pinned id as it is. Every applied
//! update is announced once as `harness.manifest_loaded {id, version, source: remote, serial}`
//! ([`announce_loaded`]). Flow: fetch `index.json` + `index.json.minisig` → verify the signature against keys
//! compiled into the binary → `serial` must strictly increase (no rollback) → fetch each listed
//! manifest whose id is a built-in harness and whose `min_vibeke..max_vibeke` covers this build
//! → sha256 must match the index → stage, then swap into `<state>/manifests/remote/` atomically.
//! The loader (`vk_agents::manifest::load`) strips process-spawning fields and unattested
//! capability rows from remote manifests.
//!
//! The index must carry a minisign signature (`<index>.minisig`) by one of the embedded release
//! keys (`vk_remote::bootstrap::TRUSTED_KEYS`, current + next: the channel has no key of its
//! own). An unsigned or wrongly signed index is refused unless the user opts in to unsigned
//! development indexes with `VIBEKE_ALLOW_UNSIGNED_MANIFESTS=1` (sha256 and serial checks still
//! apply).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;
use std::path::{Path, PathBuf};

pub const DEFAULT_URL: &str = "https://manifests.vibeke.dev/v1/stable/index.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelError {
    Disabled,
    /// The index has no `.minisig` next to it.
    Unsigned,
    /// The signature is missing a trusted key or does not verify.
    BadSignature(String),
    Rollback {
        have: u64,
        got: u64,
    },
    Checksum {
        id: String,
    },
    Fetch(String),
    Invalid(String),
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelError::Disabled => {
                write!(f, "manifest channel disabled (VIBEKE_MANIFEST_CHANNEL=0)")
            }
            ChannelError::Unsigned => write!(
                f,
                "the manifest index is not signed (no .minisig); expected a signature by {}; refusing \
                 (VIBEKE_ALLOW_UNSIGNED_MANIFESTS=1 accepts it for development)",
                vk_remote::bootstrap::expected_keys_hint()
            ),
            ChannelError::BadSignature(e) => write!(
                f,
                "the manifest index signature is not valid: {e}; refusing \
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
    /// Provenance per manifest id (`vibeke integration list --sources`).
    #[serde(default)]
    pub sources: std::collections::BTreeMap<String, SourceInfo>,
}

/// Where one cached remote manifest came from.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct SourceInfo {
    pub version: String,
    pub sha256: String,
    /// Index serial it was fetched with.
    pub serial: u64,
    pub fetched_at_ms: i64,
}

/// A frozen manifest: updates keep the cached copy as it is.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct Pin {
    pub version: String,
    pub pinned_at_ms: i64,
}

pub fn pins_path(root: &Path) -> PathBuf {
    root.join("pins.json")
}

pub fn read_pins(root: &Path) -> std::collections::BTreeMap<String, Pin> {
    std::fs::read_to_string(pins_path(root))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_pins(
    root: &Path,
    pins: &std::collections::BTreeMap<String, Pin>,
) -> Result<(), ChannelError> {
    std::fs::create_dir_all(root).map_err(|e| ChannelError::Fetch(e.to_string()))?;
    let tmp = root.join("pins.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(pins).unwrap_or_default())
        .and_then(|_| std::fs::rename(&tmp, pins_path(root)))
        .map_err(|e| ChannelError::Fetch(e.to_string()))
}

/// Freeze the cached remote manifest `id` at `version` (`None` or `"current"`: whatever is
/// cached now). Only a version that is actually cached can be pinned.
pub fn pin(root: &Path, id: &str, version: Option<&str>) -> Result<Pin, ChannelError> {
    let state = read_state(root).ok_or_else(|| {
        ChannelError::Invalid(
            "no remote manifests installed (run `vibeke integration update`)".into(),
        )
    })?;
    let have = state
        .sources
        .get(id)
        .map(|s| s.version.clone())
        .or_else(|| {
            state
                .applied
                .iter()
                .find_map(|a| a.strip_prefix(&format!("{id}@")).map(str::to_string))
        })
        .ok_or_else(|| ChannelError::Invalid(format!("{id} has no cached remote manifest")))?;
    let want = match version {
        None | Some("") | Some("current") => have.clone(),
        Some(v) => v.to_string(),
    };
    if want != have {
        return Err(ChannelError::Invalid(format!(
            "{id}: version {want} is not cached (cached: {have})"
        )));
    }
    let mut pins = read_pins(root);
    let p = Pin {
        version: want,
        pinned_at_ms: vk_store::now_ms(),
    };
    pins.insert(id.to_string(), p.clone());
    write_pins(root, &pins)?;
    Ok(p)
}

/// Remove a pin; `true` when there was one.
pub fn unpin(root: &Path, id: &str) -> Result<bool, ChannelError> {
    let mut pins = read_pins(root);
    let had = pins.remove(id).is_some();
    if had {
        write_pins(root, &pins)?;
    }
    Ok(had)
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

/// Verify the index signature against `keys` (the embedded release keys in production).
pub fn verify_signature_with(
    keys: &[String],
    index: &[u8],
    sig: Option<&[u8]>,
) -> Result<(), ChannelError> {
    let sig = sig.ok_or(ChannelError::Unsigned)?;
    let sig = String::from_utf8_lossy(sig);
    vk_remote::bootstrap::verify_signature_bytes(keys, index, &sig)
        .map(|_| ())
        .map_err(|e| ChannelError::BadSignature(e.to_string()))
}

/// [`verify_signature_with`] against the embedded release keys (current and next).
pub fn verify_signature(index: &[u8], sig: Option<&[u8]>) -> Result<(), ChannelError> {
    verify_signature_with(&vk_remote::bootstrap::trusted_keys(), index, sig)
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
    update_with(url, root, &vk_remote::bootstrap::trusted_keys())
}

/// [`update`] against explicit trusted `keys` (tests use a throwaway key).
pub fn update_with(url: &str, root: &Path, keys: &[String]) -> Result<Report, ChannelError> {
    if std::env::var("VIBEKE_MANIFEST_CHANNEL").is_ok_and(|v| v == "0") {
        return Err(ChannelError::Disabled);
    }
    let index_bytes = fetch(url)?;
    let sig = fetch(&format!("{url}.minisig")).ok();
    let mut report = Report::default();
    match verify_signature_with(keys, &index_bytes, sig.as_deref()) {
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
    let pins = read_pins(root);
    let prev = read_state(root).unwrap_or_default();
    let mut sources = prev.sources.clone();
    // Frozen manifests keep their cached file, whatever the index says.
    for (id, pin) in &pins {
        let cached = root.join("remote").join(format!("{id}.toml"));
        if cached.is_file() {
            std::fs::copy(&cached, staging.join(format!("{id}.toml")))
                .map_err(|e| ChannelError::Fetch(e.to_string()))?;
            report
                .skipped
                .push(format!("{id} (pinned at {})", pin.version));
        } else {
            sources.remove(id);
        }
    }
    sources.retain(|id, _| {
        pins.contains_key(id) && root.join("remote").join(format!("{id}.toml")).is_file()
    });
    for e in &index.manifests {
        // Unknown harness ids are skipped silently (04 §13).
        if !builtin.contains(&e.id) || pins.contains_key(&e.id) {
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
        sources.insert(
            e.id.clone(),
            SourceInfo {
                version: e.version.clone(),
                sha256: got,
                serial: index.serial,
                fetched_at_ms: vk_store::now_ms(),
            },
        );
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
        sources,
    };
    let tmp = root.join("remote-state.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&st).unwrap_or_default())
        .and_then(|_| std::fs::rename(&tmp, state_path(root)))
        .map_err(|e| ChannelError::Fetch(e.to_string()))?;
    report.serial = index.serial;
    Ok(report)
}

/// kv scope/key remembering the last serial announced as `harness.manifest_loaded`.
const KV_ANNOUNCED: (&str, &str) = ("manifests", "announced_serial");

/// Emit `harness.manifest_loaded {id, version, source: remote, serial}` once per applied
/// manifest of the cached update (04 §13 audit). Returns the number of events.
pub fn announce_loaded(server: &crate::Server) -> usize {
    announce_loaded_from(server, &root())
}

pub fn announce_loaded_from(server: &crate::Server, root: &Path) -> usize {
    let Some(st) = read_state(root) else { return 0 };
    let seen: u64 = server
        .with_core(|c| {
            c.store
                .kv_get(KV_ANNOUNCED.0, KV_ANNOUNCED.1)
                .ok()
                .flatten()
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if st.serial <= seen {
        return 0;
    }
    let mut n = 0;
    server.with_core(|c| {
        let mut tx = crate::core::Tx::new();
        for a in &st.applied {
            let (id, version) = a.split_once('@').unwrap_or((a, ""));
            tx.event(
                "harness.manifest_loaded",
                serde_json::json!({"manifest": id}),
                serde_json::json!({"id": id, "version": version, "source": "remote", "serial": st.serial, "verified": st.verified}),
            );
            n += 1;
        }
        tx.m.kv(KV_ANNOUNCED.0, KV_ANNOUNCED.1, Some(st.serial.to_string()));
        let _ = c.commit(tx);
    });
    n
}

/// Seconds between polls (6 h, 04 §13); `VIBEKE_MANIFEST_POLL_SECS` overrides for tests.
pub fn poll_interval() -> std::time::Duration {
    std::time::Duration::from_secs(
        std::env::var("VIBEKE_MANIFEST_POLL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(6 * 3600),
    )
}

/// First poll comes this long after the server starts.
pub fn first_poll_delay() -> std::time::Duration {
    std::time::Duration::from_secs(
        std::env::var("VIBEKE_MANIFEST_FIRST_POLL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15 * 60),
    )
}

/// `[update] channel` (`stable` | `preview`) → index URL.
pub fn channel_url(cfg: &vk_config::Config) -> String {
    match cfg.update.channel {
        vk_config::UpdateChannel::Stable => DEFAULT_URL.to_string(),
        _ => DEFAULT_URL.replace("/stable/", "/preview/"),
    }
}

/// One poll: fetch, verify and apply, then announce what was loaded. Failures are logged, never
/// fatal (the cached set stays in force).
pub fn poll_once(
    server: &crate::Server,
    url: &str,
    root: &Path,
    keys: &[String],
) -> Option<Report> {
    match update_with(url, root, keys) {
        Ok(r) => {
            tracing::info!(serial = r.serial, applied = ?r.applied, "manifest channel updated");
            let n = announce_loaded_from(server, root);
            // The registry re-reads the cache on the next reload.
            if n > 0 || !r.applied.is_empty() {
                let _ = super::manifests::reload();
            }
            Some(r)
        }
        // Not newer than the cache is the normal quiet case.
        Err(ChannelError::Rollback { .. }) => None,
        Err(e) => {
            tracing::debug!("manifest poll: {e}");
            None
        }
    }
}

/// Background poller (`[update] manifest_check`, default on). Never runs in unit tests, and
/// `VIBEKE_MANIFEST_CHANNEL=0` switches it off.
pub fn start_polling(server: &std::sync::Arc<crate::Server>) {
    if cfg!(test) {
        return;
    }
    let srv = server.clone();
    tokio::spawn(async move {
        tokio::time::sleep(first_poll_delay()).await;
        loop {
            let cfg = vk_config::Config::load(vk_config::config_path())
                .map(|(c, _)| c)
                .unwrap_or_default();
            if cfg.update.manifest_check
                && !std::env::var("VIBEKE_MANIFEST_CHANNEL").is_ok_and(|v| v == "0")
            {
                let url = channel_url(&cfg);
                let s2 = srv.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    poll_once(&s2, &url, &root(), &vk_remote::bootstrap::trusted_keys())
                })
                .await;
            }
            tokio::time::sleep(poll_interval()).await;
        }
    });
}

/// `vibeke integration channel` status as JSON.
pub fn status_json(root: &Path) -> Value {
    match read_state(root) {
        Some(s) => {
            serde_json::json!({"serial": s.serial, "created_at": s.created_at, "verified": s.verified, "applied": s.applied, "url": s.url, "sources": s.sources, "pins": read_pins(root)})
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
        // No signature: refused, and the message names the expected key ids.
        let e = update(&url, root.path()).unwrap_err();
        assert_eq!(e, ChannelError::Unsigned);
        assert!(e.to_string().contains("5F6E09C78F555F34"), "{e}");
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

    #[test]
    fn signed_index_accepted_only_with_a_trusted_key() {
        use vk_remote::minisign::testing;
        let _g = ENV.lock().unwrap();
        // SAFETY: test-only env mutation, serialized by ENV.
        unsafe { std::env::remove_var("VIBEKE_ALLOW_UNSIGNED_MANIFESTS") };
        let srv = tempfile::tempdir().unwrap();
        let body =
            "id = \"gemini\"\n[[screen.rules]]\nid = \"x\"\nstate = \"working\"\nany = ['busy']\n";
        let url = publish(srv.path(), 1, body, false);
        let index = std::fs::read(srv.path().join("index.json")).unwrap();
        let sig = testing::sign(&index, "timestamp:1 file:index.json");
        std::fs::write(srv.path().join("index.json.minisig"), &sig).unwrap();
        let keys = vec![testing::public_key_b64()];

        // The real embedded keys refuse a signature by another key (and name themselves).
        let root = tempfile::tempdir().unwrap();
        let e = update(&url, root.path()).unwrap_err();
        assert!(matches!(e, ChannelError::BadSignature(_)), "{e}");
        assert!(e.to_string().contains("69536A23D04E2C7C"), "{e}");
        assert!(read_state(root.path()).is_none());

        // A trusted key accepts it; the state records a verified signature.
        let r = update_with(&url, root.path(), &keys).unwrap();
        assert!(!r.unsigned);
        assert_eq!(read_state(root.path()).unwrap().verified, "signature");

        // A tampered index no longer verifies.
        let root2 = tempfile::tempdir().unwrap();
        std::fs::write(
            srv.path().join("index.json"),
            String::from_utf8_lossy(&index).replace("\"serial\":1", "\"serial\":9"),
        )
        .unwrap();
        assert!(matches!(
            update_with(&url, root2.path(), &keys),
            Err(ChannelError::BadSignature(_))
        ));
    }
}
