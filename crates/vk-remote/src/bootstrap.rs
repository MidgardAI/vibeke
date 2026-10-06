//! No-sudo bootstrap (06 A3): probe the remote, push the matching verified artifact into
//! `~/.local/share/vibeke/versions/<v>/`, re-check its sha256 there, and switch the `current`
//! symlink atomically. Never touches system paths; never uses sudo.

use crate::ssh::{Target, sh_quote};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub const REMOTE_BIN: &str = "~/.local/share/vibeke/current/vibeke";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub os: String,
    pub arch: String,
    pub home: String,
    pub version: Option<String>,
    pub libc: String,
}

impl Probe {
    /// Release target name: `linux-x86_64`, `linux-aarch64`, `macos-aarch64`, …
    pub fn target(&self) -> String {
        let os = match self.os.as_str() {
            "Linux" => "linux",
            "Darwin" => "macos",
            o => o,
        };
        let arch = match self.arch.as_str() {
            "arm64" | "aarch64" => "aarch64",
            "x86_64" | "amd64" => "x86_64",
            a => a,
        };
        format!("{os}-{arch}")
    }
}

const PROBE: &str = r#"uname -s; uname -m; echo "$HOME"
B="$HOME/.local/share/vibeke/current/vibeke"
if [ -x "$B" ]; then "$B" --version 2>/dev/null | head -1; else echo none; fi
if ldd --version 2>&1 | grep -qi musl; then echo musl; elif ldd --version >/dev/null 2>&1; then echo glibc; else echo unknown; fi
"#;

pub async fn probe(t: &Target) -> Result<Probe> {
    let out = t.run("sh -s", Some(PROBE.as_bytes())).await?;
    let l: Vec<&str> = out.lines().collect();
    if l.len() < 4 {
        bail!("unexpected probe output from {}: {out:?}", t.address);
    }
    let version = l[3].strip_prefix("vibeke ").map(|v| v.trim().to_string());
    Ok(Probe {
        os: l[0].trim().into(),
        arch: l[1].trim().into(),
        home: l[2].trim().into(),
        version,
        libc: l.get(4).unwrap_or(&"unknown").trim().into(),
    })
}

/// How an artifact earned trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Checksum from `SHA256SUMS`, whose minisign signature verified against embedded keys.
    Signed,
    /// Checksum from a sidecar / `SHA256SUMS` next to the artifact, no verified signature;
    /// accepted only because the user set `VIBEKE_ALLOW_UNSIGNED=1`.
    UnsignedOptIn,
    /// The running binary pushed as-is: no external checksum exists, so the hash only guards
    /// the transfer. Accepted only with `VIBEKE_ALLOW_UNSIGNED=1`.
    SelfHashedOptIn,
}

#[derive(Debug, Clone)]
pub struct Artifact {
    pub path: PathBuf,
    pub sha256: String,
    pub version: String,
    pub trust: Trust,
}

pub fn sha256_file(path: &std::path::Path) -> Result<String> {
    let data = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&data)))
}

/// Verify an artifact against its recorded checksum.
pub fn verify(a: &Artifact) -> Result<()> {
    let actual = sha256_file(&a.path)?;
    if actual != a.sha256 {
        bail!(
            "checksum mismatch for {}: expected {}, got {actual}",
            a.path.display(),
            a.sha256
        );
    }
    Ok(())
}

/// The current release signing key (`keys/vibeke-2026.pub`, minisign key id `5F6E09C78F555F34`).
/// `scripts/release-sign.sh` reads this line to verify what it signed, so keep it on one line.
pub const KEY_CURRENT: &str = "RWQ0X1WPxwluX2gFO4vO586PSTdpSfJqrb+xsQnZ2ctND/VDw7VCWx5z";
/// The next release signing key (`keys/vibeke-next.pub`, key id `69536A23D04E2C7C`), embedded one
/// release ahead of its first use so a rotation never needs a flag day (09 §10).
pub const KEY_NEXT: &str = "RWR8LE7QI2pTaSsb4srEFbF1j78fXZzbORy4KGRzHErddJwSJLxwqH3x";

/// An embedded release public key.
#[derive(Debug, Clone, Copy)]
pub struct ReleaseKey {
    pub label: &'static str,
    /// minisign key id as printed by `minisign` (upper-case hex).
    pub key_id: &'static str,
    pub public_key: &'static str,
}

pub const RELEASE_KEYS: &[ReleaseKey] = &[
    ReleaseKey {
        label: "current",
        key_id: "5F6E09C78F555F34",
        public_key: KEY_CURRENT,
    },
    ReleaseKey {
        label: "next",
        key_id: "69536A23D04E2C7C",
        public_key: KEY_NEXT,
    },
];

/// Minisign public keys (base64 key lines, current + next for rotation, 09 §10) trusted to
/// sign `SHA256SUMS` and the release manifest. The manifest channel uses the same keys.
pub const TRUSTED_KEYS: &[&str] = &[KEY_CURRENT, KEY_NEXT];

/// `release key 5F6E09C78F555F34 (current) or 69536A23D04E2C7C (next)`, for error messages.
pub fn expected_keys_hint() -> String {
    let ids: Vec<String> = RELEASE_KEYS
        .iter()
        .map(|k| format!("{} ({})", k.key_id, k.label))
        .collect();
    format!("release key {}", ids.join(" or "))
}

/// The keys signatures are checked against: [`TRUSTED_KEYS`], plus the deterministic test
/// key in `cfg(test)` builds only (`minisign::testing`), never in a shipped binary.
pub fn trusted_keys() -> Vec<String> {
    #[allow(unused_mut)]
    let mut keys: Vec<String> = TRUSTED_KEYS.iter().map(|k| k.to_string()).collect();
    #[cfg(test)]
    keys.push(crate::minisign::testing::public_key_b64());
    keys
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureError {
    /// No keys were given to verify against (never the case for the embedded release keys).
    NoTrustedKeys,
    /// The signature or checksum file could not be read.
    Io(String),
    /// The signature is malformed, from an untrusted key, or does not verify.
    Invalid(crate::minisign::MinisignError),
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignatureError::NoTrustedKeys => {
                write!(
                    f,
                    "no release signing keys to verify against (expected {})",
                    expected_keys_hint()
                )
            }
            SignatureError::Io(e) => write!(f, "{e}"),
            SignatureError::Invalid(crate::minisign::MinisignError::UnknownKey { key_id }) => {
                write!(
                    f,
                    "signed with untrusted key {key_id}; expected {}",
                    expected_keys_hint()
                )
            }
            SignatureError::Invalid(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SignatureError {}

/// Verify minisign signature text `sig` over `data` against `keys`. Returns the verified
/// trusted comment.
pub fn verify_signature_bytes(
    keys: &[String],
    data: &[u8],
    sig: &str,
) -> Result<String, SignatureError> {
    if keys.is_empty() {
        return Err(SignatureError::NoTrustedKeys);
    }
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    crate::minisign::verify(&refs, data, sig).map_err(SignatureError::Invalid)
}

/// Verify the minisign signature `sig` over the checksum file `sums` with `keys`.
pub fn verify_signature_with(
    keys: &[String],
    sums: &std::path::Path,
    sig: &std::path::Path,
) -> Result<(), SignatureError> {
    if keys.is_empty() {
        return Err(SignatureError::NoTrustedKeys);
    }
    let data =
        std::fs::read(sums).map_err(|e| SignatureError::Io(format!("{}: {e}", sums.display())))?;
    let sig_text = std::fs::read_to_string(sig)
        .map_err(|e| SignatureError::Io(format!("{}: {e}", sig.display())))?;
    verify_signature_bytes(keys, &data, &sig_text).map(|_| ())
}

/// Verify the minisign signature `sig` over the checksum file `sums` against
/// [`trusted_keys`] (the embedded current and next release keys). Anything else is refused,
/// and the opt-in path (`VIBEKE_ALLOW_UNSIGNED=1`) is the only way to accept artifacts.
pub fn verify_signature(
    sums: &std::path::Path,
    sig: &std::path::Path,
) -> Result<(), SignatureError> {
    verify_signature_with(&trusted_keys(), sums, sig)
}

/// One target's entry in the signed release manifest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ManifestArtifact {
    /// Release target (`linux-x86_64`, `linux-aarch64-musl`, …).
    pub target: String,
    pub sha256: String,
    /// Download URL (`remote-download` mode); the remote checks `sha256` after `curl`.
    #[serde(default)]
    pub url: String,
}

/// The release manifest (`manifest.json` + `manifest.json.minisig`) that
/// `bootstrap = "remote-download"` trusts (06 A3): verified on the laptop, the remote only
/// gets the expected sha256.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct ReleaseManifest {
    pub version: String,
    pub artifacts: Vec<ManifestArtifact>,
}

impl ReleaseManifest {
    pub fn artifact(&self, target: &str) -> Option<&ManifestArtifact> {
        self.artifacts.iter().find(|a| a.target == target)
    }
}

/// Verify and parse a signed release manifest. The signature's trusted comment must name the
/// manifest's version (`… version:<v> …`), so an old signed manifest cannot be replayed
/// under a new version label.
pub fn verify_manifest_with(
    keys: &[String],
    manifest: &[u8],
    sig: &str,
) -> Result<ReleaseManifest, String> {
    let comment = verify_signature_bytes(keys, manifest, sig).map_err(|e| e.to_string())?;
    let m: ReleaseManifest =
        serde_json::from_slice(manifest).map_err(|e| format!("release manifest: {e}"))?;
    if !comment
        .split_whitespace()
        .any(|w| w == format!("version:{}", m.version))
    {
        return Err(format!(
            "release manifest signature is for {comment:?}, not version {}",
            m.version
        ));
    }
    for a in &m.artifacts {
        if !valid_sha(&a.sha256) {
            return Err(format!("release manifest: bad sha256 for {}", a.target));
        }
    }
    Ok(m)
}

/// [`verify_manifest_with`] against [`trusted_keys`].
pub fn verify_manifest(manifest: &[u8], sig: &str) -> Result<ReleaseManifest, String> {
    verify_manifest_with(&trusted_keys(), manifest, sig)
}

/// `VIBEKE_ALLOW_UNSIGNED=1`: the user explicitly accepts unsigned development builds.
pub fn allow_unsigned_env() -> bool {
    std::env::var("VIBEKE_ALLOW_UNSIGNED").is_ok_and(|v| v == "1")
}

pub fn unsigned_warning(path: &std::path::Path, sha256: &str) -> String {
    format!(
        "WARNING: VIBEKE_ALLOW_UNSIGNED=1: using UNSIGNED development artifact {} (sha256 {sha256}); \
         its integrity is not backed by a verified signature",
        path.display()
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SumSource {
    Sums,
    Sidecar,
}

pub(crate) fn valid_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Expected sha256 for `artifact`, from `SHA256SUMS` in the same directory, else the
/// `<artifact>.sha256` sidecar. Never derived from the artifact itself.
fn expected_sha256(artifact: &std::path::Path) -> Result<(String, SumSource)> {
    let name = artifact
        .file_name()
        .and_then(|n| n.to_str())
        .context("artifact name")?;
    let dir = artifact.parent().unwrap_or(std::path::Path::new("."));
    if let Ok(text) = std::fs::read_to_string(dir.join("SHA256SUMS")) {
        for line in text.lines() {
            let mut it = line.split_whitespace();
            if let (Some(h), Some(n)) = (it.next(), it.next())
                && n.trim_start_matches('*') == name
                && valid_sha(h)
            {
                return Ok((h.to_ascii_lowercase(), SumSource::Sums));
            }
        }
    }
    let mut sc = artifact.as_os_str().to_owned();
    sc.push(".sha256");
    if let Ok(text) = std::fs::read_to_string(&sc)
        && let Some(h) = text.split_whitespace().next()
        && valid_sha(h)
    {
        return Ok((h.to_ascii_lowercase(), SumSource::Sidecar));
    }
    bail!(
        "no checksum for {name}: expected an entry in SHA256SUMS or a {name}.sha256 sidecar next to it"
    )
}

/// Decide whether `artifact` may be installed or executed. Returns the verified sha256.
/// (a) the expected checksum must come from a file next to it, and match; and
/// (b) `SHA256SUMS.minisig` must verify, or `allow_unsigned` must be set.
pub fn trust_artifact(artifact: &std::path::Path, allow_unsigned: bool) -> Result<(String, Trust)> {
    trust_artifact_signed(artifact, allow_unsigned).map(|(sha, trust, _)| (sha, trust))
}

/// Whether a verified trusted comment names `version`: `vibeke v<version>` (what
/// `scripts/release-sign.sh` writes for `SHA256SUMS`, optionally followed by more words) or a
/// `version:<version>` word (the manifest's form).
pub fn comment_names_version(comment: &str, version: &str) -> bool {
    let words: Vec<&str> = comment.split_whitespace().collect();
    let tagged = format!("v{version}");
    let keyed = format!("version:{version}");
    (words.len() >= 2 && words[0] == "vibeke" && words[1] == tagged)
        || words.iter().any(|w| *w == keyed)
}

/// A signed artifact must be signed *for* `version` (09 §10): an older release's valid
/// `SHA256SUMS` signature can't be replayed under a newer version. Unsigned opt-in artifacts
/// have no signed version to compare (their version is checked by the caller).
pub fn require_signed_version(trust: Trust, comment: Option<&str>, version: &str) -> Result<()> {
    if trust != Trust::Signed {
        return Ok(());
    }
    let c = comment.unwrap_or_default();
    if !comment_names_version(c, version) {
        bail!(
            "SHA256SUMS is signed for {c:?}, not vibeke v{version} (an older or different release served as {version}?); refusing"
        );
    }
    Ok(())
}

/// [`trust_artifact`], also returning the verified trusted comment of a signed artifact.
pub fn trust_artifact_signed(
    artifact: &std::path::Path,
    allow_unsigned: bool,
) -> Result<(String, Trust, Option<String>)> {
    let (expected, source) = expected_sha256(artifact)?;
    let actual = sha256_file(artifact)?;
    if actual != expected {
        bail!(
            "checksum mismatch for {}: expected {expected}, got {actual}",
            artifact.display()
        );
    }
    let dir = artifact.parent().unwrap_or(std::path::Path::new("."));
    let sig = dir.join("SHA256SUMS.minisig");
    let sig_err = if source == SumSource::Sums && sig.is_file() {
        let verified = std::fs::read(dir.join("SHA256SUMS"))
            .map_err(|e| e.to_string())
            .and_then(|data| {
                let text = std::fs::read_to_string(&sig).map_err(|e| e.to_string())?;
                verify_signature_bytes(&trusted_keys(), &data, &text).map_err(|e| e.to_string())
            });
        match verified {
            Ok(comment) => return Ok((actual, Trust::Signed, Some(comment))),
            Err(e) => e,
        }
    } else if source == SumSource::Sums {
        format!(
            "no SHA256SUMS.minisig next to it; expected a signature by {}",
            expected_keys_hint()
        )
    } else {
        format!(
            "checksum came from a sidecar, which no signature covers; expected SHA256SUMS signed by {}",
            expected_keys_hint()
        )
    };
    if allow_unsigned {
        return Ok((actual, Trust::UnsignedOptIn, None));
    }
    bail!(
        "{} is not signed ({sig_err}); refusing. Set VIBEKE_ALLOW_UNSIGNED=1 only for development builds you made yourself",
        artifact.display()
    )
}

/// Build an [`Artifact`] from a path after [`trust_artifact`]; a signed one must be signed for
/// `version` ([`require_signed_version`]).
pub fn load_artifact(path: PathBuf, version: String, allow_unsigned: bool) -> Result<Artifact> {
    let (sha256, trust, comment) = trust_artifact_signed(&path, allow_unsigned)?;
    require_signed_version(trust, comment.as_deref(), &version)?;
    Ok(Artifact {
        path,
        sha256,
        version,
        trust,
    })
}

/// The running binary as an artifact. Requires the opt-in because there is no independent
/// checksum for it; the hash computed here only protects the transfer.
pub fn load_self_artifact(exe: PathBuf, version: String, allow_unsigned: bool) -> Result<Artifact> {
    if let Ok(a) = load_artifact(exe.clone(), version.clone(), allow_unsigned) {
        return Ok(a);
    }
    if !allow_unsigned {
        bail!("pushing the running binary requires VIBEKE_ALLOW_UNSIGNED=1");
    }
    Ok(Artifact {
        sha256: sha256_file(&exe)?,
        path: exe,
        version,
        trust: Trust::SelfHashedOptIn,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    AlreadyCurrent,
    Installed,
    Upgraded,
}

/// Make sure the remote runs `artifact.version`. `upgrade_ok` gates replacing another version.
pub async fn ensure(
    t: &Target,
    p: &Probe,
    artifact: Option<&Artifact>,
    upgrade_ok: bool,
    allow_downgrade: bool,
) -> Result<Outcome> {
    let want = artifact.map(|a| a.version.clone());
    if let Some(w) = &want {
        downgrade_check(&t.label, p.version.as_deref(), w, allow_downgrade)?;
    }
    match (&p.version, &want) {
        (Some(have), Some(w)) if have == w => return Ok(Outcome::AlreadyCurrent),
        (Some(_), None) => return Ok(Outcome::AlreadyCurrent),
        (None, None) => bail!(
            "vibeke is not installed on {} and no {} artifact is available locally (build one with `mise run dist`)",
            t.label,
            p.target()
        ),
        (Some(have), Some(w)) if !upgrade_ok => bail!(
            "{} runs vibeke {have}, local is {w}; rerun with --upgrade (panes survive the upgrade)",
            t.label
        ),
        _ => {}
    }
    let a = artifact.context("artifact")?;
    stage(t, a).await?;
    activate(t, a).await?;
    Ok(if p.version.is_some() {
        Outcome::Upgraded
    } else {
        Outcome::Installed
    })
}

/// `bootstrap = "remote-download"` (06 A3): install the version named by the **verified**
/// release manifest `m` on the remote. The laptop has already checked the manifest's
/// signature; the remote downloads the file and checks the manifest's sha256 before the atomic
/// switch. A GitHub `token` (private release repo) reaches the remote only on the stdin of
/// `sh -s`, never on a command line.
pub async fn ensure_download(
    t: &Target,
    p: &Probe,
    m: &ReleaseManifest,
    token: Option<&crate::download::Secret>,
    upgrade_ok: bool,
    allow_downgrade: bool,
) -> Result<Outcome> {
    let entry = m
        .artifact(&p.target())
        .with_context(|| format!("the release manifest has no {} artifact", p.target()))?;
    downgrade_check(&t.label, p.version.as_deref(), &m.version, allow_downgrade)?;
    match &p.version {
        Some(have) if *have == m.version => return Ok(Outcome::AlreadyCurrent),
        Some(have) if !upgrade_ok => bail!(
            "{} runs vibeke {have}, the release is {}; rerun with --upgrade (panes survive the upgrade)",
            t.label,
            m.version
        ),
        _ => {}
    }
    if !valid_version(&m.version) || !valid_sha(&entry.sha256) {
        bail!("invalid version or sha256 in the release manifest");
    }
    let api_base = crate::download::github_api_base();
    let (url, octet) = crate::download::resolve_download(&entry.url, token, &api_base)?;
    let dir = format!("~/.local/share/vibeke/versions/{}", m.version);
    let script = crate::download::remote_download_script(
        &sh_quote(&dir),
        &url,
        octet,
        crate::download::token_for(&url, token.filter(|_| octet), &api_base),
        &entry.sha256,
    )?;
    let out = t
        .run("sh -s", Some(script.as_bytes()))
        .await
        .context("remote download")?;
    if !out.contains("staged") {
        bail!("remote download check failed: {out}");
    }
    activate_staged(t, &m.version, &entry.sha256).await?;
    Ok(if p.version.is_some() {
        Outcome::Upgraded
    } else {
        Outcome::Installed
    })
}

fn check_artifact(a: &Artifact) -> Result<()> {
    if !valid_sha(&a.sha256) {
        bail!("invalid sha256 for artifact");
    }
    if !valid_version(&a.version) {
        bail!("invalid artifact version {:?}", a.version);
    }
    verify(a)
}

/// `a` is an older version than `b` (numeric dotted parts; a pre-release sorts before its
/// release; build metadata is ignored).
pub fn version_older(a: &str, b: &str) -> bool {
    fn split(v: &str) -> (Vec<u64>, Option<&str>) {
        let v = v.split('+').next().unwrap_or(v);
        let (core, pre) = match v.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (v, None),
        };
        (
            core.split('.').map(|x| x.parse().unwrap_or(0)).collect(),
            pre,
        )
    }
    let ((mut x, xp), (mut y, yp)) = (split(a), split(b));
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    match x.cmp(&y) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => match (xp, yp) {
            (Some(_), None) => true,
            (Some(p), Some(q)) => p < q,
            _ => false,
        },
    }
}

/// Refuse to replace `have` on `label` with the older `want` unless explicitly allowed
/// (`--allow-downgrade`): a replayed old release must not roll a machine back silently.
pub fn downgrade_check(label: &str, have: Option<&str>, want: &str, allow: bool) -> Result<()> {
    if let Some(have) = have
        && version_older(want, have)
        && !allow
    {
        bail!(
            "{label} runs vibeke {have}, newer than {want}; refusing to downgrade (pass --allow-downgrade to do it on purpose)"
        );
    }
    Ok(())
}

pub(crate) fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

/// Stage `a` on the remote without switching to it: upload into
/// `~/.local/share/vibeke/versions/<v>/vibeke.tmp`, re-check its sha256 there and only then
/// rename it to `vibeke`. The running version (`current`) is untouched, so a failed or
/// tampered transfer leaves the remote exactly as it was. Returns the staged path.
pub async fn stage(t: &Target, a: &Artifact) -> Result<String> {
    check_artifact(a)?;
    let data = std::fs::read(&a.path)?;
    let dir = format!("~/.local/share/vibeke/versions/{}", a.version);
    let upload = format!(
        "mkdir -p {d} && chmod 700 ~/.local/share/vibeke && cat > {d}/vibeke.tmp",
        d = sh_quote(&dir)
    );
    t.run(&upload, Some(&data)).await.context("upload")?;
    // sha and version are validated above (hex / [A-Za-z0-9._+-]) and quoted anyway.
    let check = format!(
        r#"set -e
cd {d}
if command -v sha256sum >/dev/null 2>&1; then echo "{sha}  vibeke.tmp" | sha256sum -c - >/dev/null
else echo "{sha}  vibeke.tmp" | shasum -a 256 -c - >/dev/null; fi
chmod 755 vibeke.tmp && mv vibeke.tmp vibeke
echo staged"#,
        d = sh_quote(&dir),
        sha = a.sha256,
    );
    let out = t
        .run("sh -s", Some(check.as_bytes()))
        .await
        .context("verify staged artifact on the remote")?;
    if !out.contains("staged") {
        bail!("staging check failed: {out}");
    }
    Ok(format!("{dir}/vibeke"))
}

/// Switch `current` to the staged version `a` (atomic where `mv -T` exists), link
/// `~/.local/bin/vibeke`, prune all but the last 2 versions and check the new binary runs.
pub async fn activate(t: &Target, a: &Artifact) -> Result<()> {
    check_artifact(a)?;
    activate_staged(t, &a.version, &a.sha256).await
}

/// [`activate`] for a version already staged on the remote (no local file involved).
pub async fn activate_staged(t: &Target, version: &str, sha256: &str) -> Result<()> {
    if !valid_sha(sha256) || !valid_version(version) {
        bail!("invalid sha256 or version for activation");
    }
    let dir = format!("~/.local/share/vibeke/versions/{version}");
    let activate = format!(
        r#"set -e
V={v}
cd {d}
if command -v sha256sum >/dev/null 2>&1; then echo "{sha}  vibeke" | sha256sum -c - >/dev/null
else echo "{sha}  vibeke" | shasum -a 256 -c - >/dev/null; fi
cd ~/.local/share/vibeke
rm -f current.new
ln -s "versions/$V" current.new
# Atomic rename over the symlink where `mv -T` exists (GNU coreutils, modern busybox).
# Fallback (BSD/macOS mv has no -T): `ln -sfn` is NOT atomic, but `current` still only ever
# names a fully written, checksum-verified version directory.
if ! mv -Tf current.new current 2>/dev/null; then
  ln -sfn "versions/$V" current
  rm -f current.new
fi
mkdir -p ~/.local/bin && ln -sfn ~/.local/share/vibeke/current/vibeke ~/.local/bin/vibeke
# keep the last 2 versions
ls -1t versions | tail -n +3 | while read old; do [ "$old" = "$V" ] || rm -rf "versions/$old"; done
"$HOME/.local/share/vibeke/current/vibeke" --version"#,
        d = sh_quote(&dir),
        sha = sha256,
        v = sh_quote(version)
    );
    let out = t
        .run("sh -s", Some(activate.as_bytes()))
        .await
        .context("activate")?;
    if !out.contains(version) {
        bail!("activation check failed: {out}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_names() {
        let p = Probe {
            os: "Linux".into(),
            arch: "aarch64".into(),
            home: "/home/x".into(),
            version: None,
            libc: "glibc".into(),
        };
        assert_eq!(p.target(), "linux-aarch64");
        let p = Probe {
            os: "Linux".into(),
            arch: "x86_64".into(),
            ..p
        };
        assert_eq!(p.target(), "linux-x86_64");
    }

    #[test]
    fn verify_checksums() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("vibeke");
        std::fs::write(&f, b"binary").unwrap();
        let sha = sha256_file(&f).unwrap();
        assert!(
            verify(&Artifact {
                path: f.clone(),
                sha256: sha,
                version: "0.1.0".into(),
                trust: Trust::UnsignedOptIn
            })
            .is_ok()
        );
        assert!(
            verify(&Artifact {
                path: f,
                sha256: "00".into(),
                version: "0.1.0".into(),
                trust: Trust::UnsignedOptIn
            })
            .is_err()
        );
    }

    fn dist(sidecar: bool, sums: bool, good: bool) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("vibeke-linux-x86_64");
        std::fs::write(&f, b"binary").unwrap();
        let sha = if good {
            sha256_file(&f).unwrap()
        } else {
            "ab".repeat(32)
        };
        if sidecar {
            std::fs::write(
                d.path().join("vibeke-linux-x86_64.sha256"),
                format!("{sha}  x\n"),
            )
            .unwrap();
        }
        if sums {
            std::fs::write(
                d.path().join("SHA256SUMS"),
                format!("{}  other\n{sha}  vibeke-linux-x86_64\n", "cd".repeat(32)),
            )
            .unwrap();
        }
        (d, f)
    }

    #[test]
    fn checksum_never_derived_from_candidate() {
        let (_d, f) = dist(false, false, true);
        assert!(
            trust_artifact(&f, true).is_err(),
            "no sidecar -> no trust even when opted in"
        );
        let (_d, f) = dist(true, false, false);
        assert!(
            trust_artifact(&f, true).is_err(),
            "mismatch refused even when opted in"
        );
    }

    #[test]
    fn unsigned_requires_opt_in() {
        let (_d, f) = dist(true, false, true);
        assert!(trust_artifact(&f, false).is_err());
        assert_eq!(trust_artifact(&f, true).unwrap().1, Trust::UnsignedOptIn);
        let (_d, f) = dist(false, true, true);
        assert!(trust_artifact(&f, false).is_err());
        assert_eq!(trust_artifact(&f, true).unwrap().1, Trust::UnsignedOptIn);
    }

    #[test]
    fn signature_not_verifiable_without_keys() {
        let (d, f) = dist(false, true, true);
        std::fs::write(d.path().join("SHA256SUMS.minisig"), b"junk").unwrap();
        assert!(
            trust_artifact(&f, false).is_err(),
            "a signature file alone is not trust"
        );
        // With no embedded keys (the shipped state today) nothing verifies, even a valid
        // signature.
        let sums = d.path().join("SHA256SUMS");
        let good = crate::minisign::testing::sign(&std::fs::read(&sums).unwrap(), "x");
        std::fs::write(d.path().join("SHA256SUMS.minisig"), good).unwrap();
        assert_eq!(
            verify_signature_with(&[], &sums, &d.path().join("SHA256SUMS.minisig")),
            Err(SignatureError::NoTrustedKeys)
        );
        let prod: Vec<String> = TRUSTED_KEYS.iter().map(|k| k.to_string()).collect();
        assert!(
            !prod.contains(&crate::minisign::testing::public_key_b64()),
            "the test key must never be a release key"
        );
    }

    /// Review batch 2, finding 10: a correctly signed N−1 release served for N is refused (the
    /// verified trusted comment must name the requested version), and an older version never
    /// replaces a newer one without `--allow-downgrade`.
    #[test]
    fn signed_artifacts_are_bound_to_their_version_and_downgrades_refused() {
        let (d, f) = dist(false, true, true);
        let sums = d.path().join("SHA256SUMS");
        let sign = |comment: &str| {
            let sig = crate::minisign::testing::sign(&std::fs::read(&sums).unwrap(), comment);
            std::fs::write(d.path().join("SHA256SUMS.minisig"), sig).unwrap();
        };
        sign("vibeke v0.8.0");
        let e = load_artifact(f.clone(), "0.9.0".into(), false).unwrap_err();
        assert!(
            format!("{e:#}").contains("signed for \"vibeke v0.8.0\""),
            "{e:#}"
        );
        // The opt-in does not waive a valid signature for another version.
        assert!(load_artifact(f.clone(), "0.9.0".into(), true).is_err());
        assert_eq!(
            load_artifact(f.clone(), "0.8.0".into(), false)
                .unwrap()
                .trust,
            Trust::Signed
        );
        sign("vibeke v0.9.0");
        assert!(load_artifact(f.clone(), "0.9.0".into(), false).is_ok());
        // A longer version that merely starts with the requested one is not it.
        sign("vibeke v0.9.0.1");
        assert!(load_artifact(f.clone(), "0.9.0".into(), false).is_err());
        assert!(comment_names_version(
            "vibeke v1.2.3 version:1.2.3",
            "1.2.3"
        ));
        assert!(comment_names_version("timestamp:1 version:1.2.3", "1.2.3"));
        assert!(!comment_names_version("vibeke v1.2.30", "1.2.3"));
        assert!(!comment_names_version("not vibeke v1.2.3", "1.2.3"));

        assert!(version_older("0.8.0", "0.9.0"));
        assert!(version_older("0.9.0", "0.10.0"));
        assert!(version_older("1.0.0-rc.1", "1.0.0"));
        assert!(!version_older("1.0.0", "1.0.0"));
        assert!(!version_older("1.0.0+build", "1.0.0"));
        assert!(!version_older("0.10.0", "0.9.0"));
        let e = downgrade_check("box", Some("0.9.0"), "0.8.0", false).unwrap_err();
        assert!(format!("{e:#}").contains("refusing to downgrade"), "{e:#}");
        assert!(downgrade_check("box", Some("0.9.0"), "0.8.0", true).is_ok());
        assert!(downgrade_check("box", Some("0.8.0"), "0.9.0", false).is_ok());
        assert!(downgrade_check("box", None, "0.8.0", false).is_ok());
    }

    #[test]
    fn signed_sums_are_trusted_and_tampering_is_not() {
        let (d, f) = dist(false, true, true);
        let sums = d.path().join("SHA256SUMS");
        let sig = crate::minisign::testing::sign(
            &std::fs::read(&sums).unwrap(),
            "timestamp:1 file:SHA256SUMS",
        );
        std::fs::write(d.path().join("SHA256SUMS.minisig"), &sig).unwrap();
        assert_eq!(trust_artifact(&f, false).unwrap().1, Trust::Signed);
        // Editing SHA256SUMS after signing (e.g. to bless another binary) breaks trust.
        let mut text = std::fs::read_to_string(&sums).unwrap();
        text.push_str(&format!("{}  extra\n", "ef".repeat(32)));
        std::fs::write(&sums, text).unwrap();
        let e = trust_artifact(&f, false).unwrap_err();
        assert!(format!("{e:#}").contains("does not verify"), "{e:#}");
        // A signature by another key is refused.
        let other = ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]);
        let forged = crate::minisign::testing::sign_with(
            &other,
            *b"someone!",
            &std::fs::read(&sums).unwrap(),
            "x",
        );
        std::fs::write(d.path().join("SHA256SUMS.minisig"), forged).unwrap();
        let e = trust_artifact(&f, false).unwrap_err();
        assert!(format!("{e:#}").contains("untrusted key"), "{e:#}");
    }

    #[test]
    fn release_manifest_verification() {
        let m = ReleaseManifest {
            version: "0.2.0".into(),
            artifacts: vec![ManifestArtifact {
                target: "linux-x86_64".into(),
                sha256: "ab".repeat(32),
                url: "https://example.invalid/vibeke-linux-x86_64".into(),
            }],
        };
        let bytes = serde_json::to_vec(&m).unwrap();
        let sig = crate::minisign::testing::sign(&bytes, "timestamp:1 version:0.2.0");
        let got = verify_manifest(&bytes, &sig).unwrap();
        assert_eq!(got, m);
        assert_eq!(
            got.artifact("linux-x86_64").unwrap().sha256,
            "ab".repeat(32)
        );
        // Signed for another version: refused (no replay under a new label).
        let old = crate::minisign::testing::sign(&bytes, "timestamp:1 version:0.1.0");
        assert!(verify_manifest(&bytes, &old).is_err());
        // No keys: refused.
        assert!(verify_manifest_with(&[], &bytes, &sig).is_err());
        // Tampered manifest: refused.
        let mut evil = m.clone();
        evil.artifacts[0].sha256 = "cd".repeat(32);
        assert!(verify_manifest(&serde_json::to_vec(&evil).unwrap(), &sig).is_err());
    }

    #[test]
    fn self_artifact_needs_opt_in() {
        let (_d, f) = dist(false, false, true);
        assert!(load_self_artifact(f.clone(), "0.1.0".into(), false).is_err());
        let a = load_self_artifact(f, "0.1.0".into(), true).unwrap();
        assert_eq!(a.trust, Trust::SelfHashedOptIn);
    }

    #[test]
    fn embedded_release_keys_parse_and_match_their_ids() {
        use crate::minisign::{PublicKey, key_id_hex};
        assert_eq!(RELEASE_KEYS.len(), 2);
        assert_eq!(RELEASE_KEYS[0].label, "current");
        assert_eq!(RELEASE_KEYS[1].label, "next");
        assert_eq!(TRUSTED_KEYS, &[KEY_CURRENT, KEY_NEXT]);
        for k in RELEASE_KEYS {
            let pk = PublicKey::parse(k.public_key).expect(k.label);
            assert_eq!(key_id_hex(&pk.key_id), k.key_id, "{}", k.label);
            assert_eq!(pk.to_base64(), k.public_key);
        }
        assert_ne!(KEY_CURRENT, KEY_NEXT);
        // The published .pub files carry the same keys.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for (file, key) in [
            ("keys/vibeke-2026.pub", KEY_CURRENT),
            ("keys/vibeke-next.pub", KEY_NEXT),
        ] {
            let text = std::fs::read_to_string(root.join(file)).unwrap();
            assert_eq!(PublicKey::parse(&text).unwrap().to_base64(), key, "{file}");
        }
    }

    /// A fixture signed with the test-key path (`minisign::testing`) verifies against the test
    /// key; the embedded release keys refuse it and the error names them.
    #[test]
    fn fixture_signed_with_test_key_verifies_only_against_the_test_key() {
        let data = b"sums\n";
        let sig = crate::minisign::testing::sign(data, "vibeke v0.1.0");
        let test_key = crate::minisign::testing::public_key_b64();
        assert_eq!(
            verify_signature_bytes(&[test_key], data, &sig).unwrap(),
            "vibeke v0.1.0"
        );
        let real: Vec<String> = TRUSTED_KEYS.iter().map(|k| k.to_string()).collect();
        let e = SignatureError::Invalid(verify_signature_bytes_raw(&real, data, &sig));
        let msg = e.to_string();
        assert!(msg.contains("untrusted key"), "{msg}");
        assert!(
            msg.contains("5F6E09C78F555F34") && msg.contains("69536A23D04E2C7C"),
            "{msg}"
        );
    }

    fn verify_signature_bytes_raw(
        keys: &[String],
        data: &[u8],
        sig: &str,
    ) -> crate::minisign::MinisignError {
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        crate::minisign::verify(&refs, data, sig).unwrap_err()
    }

    /// Rotation: a signature by the `next` key is accepted when it is embedded, and refused
    /// when only the old key is.
    #[test]
    fn rotation_accepts_the_next_key() {
        let data = b"sums\n";
        let next_sk = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let next_id = crate::minisign::PublicKey::parse(KEY_NEXT).unwrap().key_id;
        // Stand-in for the user's next key: same key id, a test-controlled secret.
        let stand_in = crate::minisign::PublicKey::from_parts(next_id, next_sk.verifying_key());
        let keys = vec![KEY_CURRENT.to_string(), stand_in.to_base64()];
        let sig = crate::minisign::testing::sign_with(&next_sk, next_id, data, "vibeke v0.2.0");
        assert!(verify_signature_bytes(&keys, data, &sig).is_ok());
        assert!(verify_signature_bytes(&[KEY_CURRENT.to_string()], data, &sig).is_err());
    }

    #[test]
    fn unsigned_refusal_names_the_expected_keys() {
        let (_d, f) = dist(false, true, true);
        let e = format!("{:#}", trust_artifact(&f, false).unwrap_err());
        assert!(
            e.contains("5F6E09C78F555F34") && e.contains("(current)"),
            "{e}"
        );
        assert!(
            e.contains("69536A23D04E2C7C") && e.contains("(next)"),
            "{e}"
        );
        assert!(e.contains("VIBEKE_ALLOW_UNSIGNED=1"), "{e}");
        // Still refused without a checksum even with the opt-in.
        let (_d, f) = dist(false, false, true);
        assert!(trust_artifact(&f, true).is_err());
    }
}
